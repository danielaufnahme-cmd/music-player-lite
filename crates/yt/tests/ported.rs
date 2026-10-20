//! The Python worker's tests (worker/tests/*.py), ported. Offline.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::Replay;
use geonix_yt::downloader::{self, attempt_clients, human_speed, is_retryable, safe_name, target_path, MAX_ATTEMPTS};
use geonix_yt::lyrics;
use geonix_yt::net::{Error, Result};
use geonix_yt::recommender::{self, Related};
use geonix_yt::ytm;
use geonix_yt::{Yt, YtConfig};
use serde_json::{json, Value};

fn offline_yt(replay: Replay) -> Yt {
    Yt::with_transport(YtConfig::default(), Arc::new(replay))
}

// ------------------------------------------------------------ test_protocol

#[tokio::test]
async fn ping_and_unknown_command() {
    let yt = offline_yt(Replay::default());
    assert_eq!(yt.handle("ping", json!({}), |_| {}).await.unwrap(), json!({"pong": true}));
    assert_eq!(yt.handle("fly", json!({}), |_| {}).await.unwrap_err(), "unknown command: 'fly'");
}

#[tokio::test]
async fn missing_fields_are_rejected() {
    let yt = offline_yt(Replay::default());
    assert_eq!(yt.handle("search", json!({"query": ""}), |_| {}).await.unwrap_err(), "missing or invalid 'query'");
    assert_eq!(yt.handle("search", json!({"query": 5}), |_| {}).await.unwrap_err(), "missing or invalid 'query'");
    assert_eq!(yt.handle("download", json!({"dest_dir": "/x"}), |_| {}).await.unwrap_err(), "missing or invalid 'video_id'");
    assert_eq!(yt.handle("playlist_info", json!({}), |_| {}).await.unwrap_err(), "missing or invalid 'url'");
}

#[tokio::test]
async fn network_errors_are_labelled() {
    let yt = offline_yt(Replay::default());
    let err = yt.handle("search", json!({"query": "x"}), |_| {}).await.unwrap_err();
    assert!(err.starts_with("Network error"), "{err}");
}

#[test]
fn handler_exception_becomes_friendly_message() {
    assert_eq!(geonix_yt::friendly_text("ERROR: Video unavailable. This video is private"), "Video unavailable");
    assert!(geonix_yt::friendly_text("<urlopen error timed out>").starts_with("Network error"));
}

#[test]
fn progress_lines_become_progress_events() {
    let p = downloader::parse_progress_line("GXPROG downloading 524288 1048576 NA 1258291.2").unwrap();
    let mut m = downloader::ProgressMapper::new();
    let now = std::time::Instant::now();
    assert_eq!(m.map_at(&p, now).unwrap(), json!({"percent": 47.5, "speed": "1.2MiB/s"}));
    assert!(m.map_at(&p, now + std::time::Duration::from_millis(100)).is_none(), "throttled to 4/s");
    let est = downloader::parse_progress_line("GXPROG downloading 100 NA 400 NA").unwrap();
    assert_eq!(m.map_at(&est, now + std::time::Duration::from_secs(1)).unwrap(), json!({"percent": 23.8, "speed": null}));
    let fin = downloader::parse_progress_line("GXPROG finished 1048576 1048576 NA NA").unwrap();
    assert_eq!(m.map_at(&fin, now).unwrap(), json!({"percent": 95.0, "speed": null}));
    assert!(downloader::parse_progress_line("[download] 5%").is_none());
}

// ------------------------------------------------------------------ test_ytm

#[test]
fn normalize_search_result() {
    let item = json!({
        "resultType": "song", "videoId": "wU26xVT_vBU", "title": "One More Time",
        "artists": [{"name": "Daft Punk"}, {"name": "Romanthony"}],
        "album": {"name": "Discovery"}, "duration": "5:21", "duration_seconds": 321,
        "thumbnails": [{"url": "https://lh3.googleusercontent.com/x=w60-h60-l90-rj", "width": 60, "height": 60},
                       {"url": "https://lh3.googleusercontent.com/x=w120-h120-l90-rj", "width": 120, "height": 120}],
    });
    assert_eq!(
        ytm::normalize_track(&item).unwrap(),
        json!({
            "video_id": "wU26xVT_vBU", "title": "One More Time", "artist": "Daft Punk, Romanthony",
            "album": "Discovery", "duration_sec": 321,
            "thumbnail_url": "https://lh3.googleusercontent.com/x=w544-h544-l90-rj",
            "file_path": null, "downloaded": false,
        })
    );
}

#[test]
fn normalize_watch_playlist_item_uses_length() {
    let item = json!({"videoId": "abc", "title": "T", "artists": [{"name": "A"}], "album": null, "length": "1:02:03", "thumbnail": []});
    let track = ytm::normalize_track(&item).unwrap();
    assert_eq!(track["duration_sec"], 3723);
    assert_eq!(track["album"], "");
    assert_eq!(track["thumbnail_url"], "");
}

#[test]
fn normalize_rejects_items_without_id() {
    assert!(ytm::normalize_track(&json!({"title": "x"})).is_none());
}

#[test]
fn extract_playlist_id() {
    assert_eq!(ytm::extract_playlist_id("https://music.youtube.com/playlist?list=PLabc_123-x").unwrap(), "PLabc_123-x");
    assert_eq!(ytm::extract_playlist_id("https://www.youtube.com/watch?v=xyz&list=OLAK5uy_k").unwrap(), "OLAK5uy_k");
    for bad in ["https://youtube.com/watch?v=abc", "https://x.com/?list=../../etc"] {
        assert!(ytm::extract_playlist_id(bad).is_err(), "{bad}");
    }
}

#[test]
fn safe_paths_and_speed() {
    assert_eq!(safe_name("AC/DC: \"Live\"?", "Unknown"), "AC_DC_ _Live__");
    assert_eq!(safe_name("...", "Unknown"), "Unknown");
    // Names Windows can't store: device names and a trailing dot after cutting.
    assert_eq!(safe_name("con", "Unknown"), "_con");
    assert_eq!(safe_name("NUL.mp3", "Unknown"), "_NUL.mp3");
    assert_eq!(safe_name("Console", "Unknown"), "Console");
    assert_eq!(safe_name(&format!("{}. tail", "a".repeat(119)), "Unknown"), "a".repeat(119));
    assert_eq!(human_speed(Some(1_258_291.0)).as_deref(), Some("1.2MiB/s"));
    assert_eq!(human_speed(None), None);
}

#[test]
fn target_path_keeps_both_when_title_exists() {
    let dir = tempdir("target");
    let first = target_path(&dir, "Artist", "", "Song", "id1");
    assert_eq!(first, dir.join("Artist").join("Singles").join("Song.mp3"));
    std::fs::create_dir_all(first.parent().unwrap()).unwrap();
    std::fs::write(&first, b"x").unwrap();
    let second = target_path(&dir, "Artist", "", "Song", "id2");
    assert_eq!(second.file_name().unwrap(), "Song [id2].mp3");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn download_errors_retry_only_when_transient() {
    assert!(is_retryable("unable to download video data: HTTP Error 403: Forbidden"));
    assert!(is_retryable("HTTP Error 429: Too Many Requests"));
    assert!(is_retryable("Read timed out."));
    assert!(!is_retryable("Video unavailable. This video is private"));
    assert!(!is_retryable("This video is unavailable"));
    assert_eq!(downloader::clean_error("ERROR: [youtube] abc: Video unavailable"), "Video unavailable");
}

#[test]
fn each_retry_uses_different_player_clients() {
    assert!(attempt_clients(1).is_none(), "first try uses yt-dlp defaults");
    let clients: Vec<_> = (2..=MAX_ATTEMPTS).map(|n| attempt_clients(n).unwrap()).collect();
    let unique: std::collections::HashSet<_> = clients.iter().collect();
    assert_eq!(unique.len(), MAX_ATTEMPTS - 1);
    let args = downloader::ytdlp_args("abc", Path::new("/tmp/x"), 1, None);
    assert!(!args.iter().any(|a| a == "--extractor-args"));
    let args = downloader::ytdlp_args("abc", Path::new("/tmp/x"), 2, None);
    assert!(args.iter().any(|a| a == "youtube:player_client=tv_simply,web_safari"));
}

#[test]
fn album_browse_id_detection() {
    assert_eq!(ytm::extract_album_browse_id("https://music.youtube.com/browse/MPREb_4pL8gzRtw1p").as_deref(), Some("MPREb_4pL8gzRtw1p"));
    assert_eq!(ytm::extract_album_browse_id("https://music.youtube.com/playlist?list=OLAK5uy_x"), None);
}

fn album_row(video_id: &str, title: &str, available: bool) -> Value {
    let mut row = json!({
        "flexColumns": [
            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
            {"musicResponsiveListItemFlexColumnRenderer": {"text": {}}}
        ],
        "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "5:20"}]}}}],
        "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {
            "playNavigationEndpoint": {"watchEndpoint": {"videoId": video_id}}}}}},
    });
    if !available {
        row["musicItemRendererDisplayPolicy"] = json!("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT");
    }
    json!({"musicResponsiveListItemRenderer": row})
}

#[tokio::test]
async fn album_tracks_inherit_album_name_and_cover() {
    let mut replay = Replay::default();
    replay.add_innertube("browse", json!({"contents": {"twoColumnBrowseResultsRenderer": {
        "tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [{"musicResponsiveHeaderRenderer": {
            "title": {"runs": [{"text": "Discovery"}]},
            "straplineTextOne": {"runs": [{"text": "Daft Punk"}]},
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [{"url": "https://img/cover=w120-h120", "width": 120}]}}},
        }}]}}}}],
        "secondaryContents": {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents": [
            album_row("aaaaaaaaaaa", "One More Time", true),
            album_row("bbbbbbbbbbb", "Gone", false),
            album_row("aaaaaaaaaaa", "dupe", true),
        ]}}]}},
    }}}));
    let info =
        offline_yt(replay).handle("playlist_info", json!({"url": "https://music.youtube.com/browse/MPREb_abc"}), |_| {}).await.unwrap();
    assert_eq!(info["title"], "Discovery");
    let ids: Vec<_> = info["tracks"].as_array().unwrap().iter().map(|t| t["video_id"].clone()).collect();
    assert_eq!(ids, vec![json!("aaaaaaaaaaa")]);
    let t = &info["tracks"][0];
    assert_eq!((t["album"].as_str(), t["artist"].as_str(), t["duration_sec"].as_i64()), (Some("Discovery"), Some("Daft Punk"), Some(320)));
    assert!(t["thumbnail_url"].as_str().unwrap().starts_with("https://img/cover"));
}

#[tokio::test]
async fn song_info_uses_watch_playlist() {
    let mut replay = Replay::default();
    replay.add_innertube(
        "next",
        json!({"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {
        "watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"musicQueueRenderer": {"content": {
            "playlistPanelRenderer": {"contents": [{"playlistPanelVideoRenderer": {
                "videoId": "ccccccccccc", "title": {"runs": [{"text": "Song"}]},
                "longBylineText": {"runs": [{"text": "A"}]}, "lengthText": {"runs": [{"text": "3:00"}]},
            }}]}}}}}}]}}}}}),
    );
    let t = offline_yt(replay).handle("song_info", json!({"video_id": "ccccccccccc"}), |_| {}).await.unwrap();
    assert_eq!((t["title"].as_str(), t["artist"].as_str(), t["duration_sec"].as_i64()), (Some("Song"), Some("A"), Some(180)));
}

// --------------------------------------------------------------- test_lyrics

#[test]
fn parse_lrc_handles_fractions_multi_tags_and_sorting() {
    let lrc = "[ar:Someone]\n[00:12.5]Second\n[00:01.20][01:00.000]Chorus\nno tag line\n[02:03]End";
    let lines = lyrics::parse_lrc(lrc);
    let times: Vec<_> = lines.iter().map(|l| l["time_ms"].as_i64().unwrap()).collect();
    let texts: Vec<_> = lines.iter().map(|l| l["text"].as_str().unwrap()).collect();
    assert_eq!(times, vec![1200, 12500, 60000, 123000]);
    assert_eq!(texts, vec!["Chorus", "Second", "Chorus", "End"]);
}

#[test]
fn clean_title_strips_edit_markers_and_detects_edits() {
    assert_eq!(lyrics::clean_title("MEANT TO BE (HARDSTYLE ULTRA SLOWED)"), "MEANT TO BE");
    assert_eq!(lyrics::clean_title("Blinding Lights (Official Audio)"), "Blinding Lights");
    assert_eq!(lyrics::clean_title("Song - Sped Up"), "Song");
    assert!(lyrics::is_edit("Song (slowed + reverb)"));
    assert!(!lyrics::is_edit("Blinding Lights"));
    assert_eq!(lyrics::first_artist("prodArvee, svdst"), "prodArvee");
}

#[test]
fn pick_prefers_synced_then_closest_duration() {
    let hits = vec![
        json!({"plainLyrics": "a", "duration": 200}),
        json!({"syncedLyrics": "[00:01]x", "duration": 260}),
        json!({"syncedLyrics": "[00:01]y", "duration": 205}),
    ];
    assert_eq!(lyrics::pick_lrclib(&hits, 200).unwrap()["syncedLyrics"], "[00:01]y");
    assert!(lyrics::pick_lrclib(&[], 200).is_none());
}

#[tokio::test]
async fn slowed_edit_gets_timestamps_stretched() {
    let replay = Replay::http_only(vec![("/search?", 200, json!([{"syncedLyrics": "[00:10.00]Hello\n[00:20.00]World", "duration": 100}]))]);
    let found = lyrics::from_lrclib(&replay, "Song (Ultra Slowed)", "Artist", 125).await.unwrap().unwrap();
    assert_eq!(found["synced"], true);
    let times: Vec<_> = found["lines"].as_array().unwrap().iter().map(|l| l["time_ms"].as_i64().unwrap()).collect();
    assert_eq!(times, vec![12500, 25000]);
    assert!(found["source"].as_str().unwrap().contains("timed"));
    let urls = replay.urls.lock().unwrap();
    assert!(!urls.iter().any(|u| u.contains("/get?")), "edits must not use exact /get lookups");
    assert!(urls[0].contains("track_name=Song&"));
}

#[tokio::test]
async fn different_cut_falls_back_to_plain() {
    let replay = Replay::http_only(vec![("lrclib", 200, json!({"syncedLyrics": "[00:10]Hi", "plainLyrics": "Hi", "duration": 300}))]);
    let found = lyrics::from_lrclib(&replay, "Song", "Artist", 200).await.unwrap().unwrap();
    assert_eq!(found, json!({"synced": false, "lines": [{"time_ms": null, "text": "Hi"}], "source": "LRCLIB"}));
}

#[tokio::test]
async fn lrclib_404_on_exact_lookup_falls_back_to_search() {
    let replay = Replay::http_only(vec![
        ("/get?", 404, Value::Null),
        ("/search?track_name", 200, json!([])),
        ("/search?q=", 200, json!([{"plainLyrics": "la\nla", "duration": 10}])),
    ]);
    let found = lyrics::from_lrclib(&replay, "Song", "Artist", 10).await.unwrap().unwrap();
    assert_eq!(found["lines"].as_array().unwrap().len(), 2);
    assert_eq!(replay.urls.lock().unwrap().len(), 3);
}

fn plain(source: &str) -> Result<Option<Value>> {
    Ok(Some(json!({"synced": false, "lines": [{"time_ms": null, "text": "p"}], "source": source})))
}

#[tokio::test]
async fn sources_fall_through_and_failures_are_transient() {
    let synced = || async { Ok(Some(json!({"synced": true, "lines": [{"time_ms": 1, "text": "s"}], "source": "LRCLIB"}))) };
    let out = lyrics::combine(|| async { plain("YT") }, synced).await;
    assert_eq!(out["source"], "LRCLIB", "synced beats plain");

    let out = lyrics::combine(|| async { Err(Error::Net("offline".into())) }, || async { Ok(None) }).await;
    assert_eq!(out["lines"], json!([]));
    assert_eq!(out["transient"], true);

    let out = lyrics::combine(|| async { Ok(None) }, || async { Ok(None) }).await;
    assert_eq!(out["transient"], false);
}

// ----------------------------------------------------------- test_recommender

fn t(vid: &str, artist: &str) -> Value {
    json!({"video_id": vid, "title": vid.to_uppercase(), "artist": artist, "album": "", "duration_sec": 200,
           "thumbnail_url": "", "file_path": null, "downloaded": false})
}

fn rel(entries: Vec<(&str, Value, Vec<Value>)>) -> Related {
    entries.into_iter().map(|(s, seed, c)| (s.to_string(), (Some(seed), c))).collect()
}

#[test]
fn songs_related_to_several_seeds_rank_first() {
    let related = rel(vec![
        ("s1", t("s1", "Seed One"), vec![t("x", "A"), t("common", "A"), t("y", "A")]),
        ("s2", t("s2", "Seed Two"), vec![t("z", "A"), t("common", "A")]),
    ]);
    let out = recommender::rank(&related, &[], 10, recommender::MAX_PER_ARTIST);
    assert_eq!(out[0]["video_id"], "common");
    assert!(out[0]["score"].as_f64() > out[1]["score"].as_f64());
}

#[test]
fn library_songs_and_seeds_are_excluded() {
    let related =
        rel(vec![("s1", t("s1", "A"), vec![t("s2", "A"), t("lib", "A"), t("new", "A")]), ("s2", t("s2", "A"), vec![t("s1", "A")])]);
    let out = recommender::rank(&related, &["lib".to_string()], 10, recommender::MAX_PER_ARTIST);
    assert_eq!(out.iter().map(|r| r["video_id"].clone()).collect::<Vec<_>>(), vec![json!("new")]);
}

#[test]
fn earlier_position_breaks_ties_and_sets_because() {
    let related =
        rel(vec![("s1", t("s1", "Daft Punk"), vec![t("a", "A"), t("b", "A")]), ("s2", t("s2", "Justice"), vec![t("b", "A"), t("a", "A")])]);
    let out = recommender::rank(&related, &[], 10, recommender::MAX_PER_ARTIST);
    let by = |id: &str| out.iter().find(|r| r["video_id"] == id).unwrap().clone();
    assert_eq!(by("a")["because_artist"], "Daft Punk");
    assert_eq!(by("b")["because_artist"], "Justice");
    assert_eq!(by("a")["because_video_id"], "s1");
}

#[test]
fn artist_cap_keeps_list_diverse() {
    let mut cands: Vec<Value> = (0..10).map(|i| t(&format!("m{i}"), "Same")).collect();
    cands.push(t("other", "Other"));
    let related = rel(vec![("s1", t("s1", "A"), cands)]);
    let out = recommender::rank(&related, &[], 10, 3);
    let artists: Vec<_> = out.iter().map(|r| r["artist"].as_str().unwrap()).collect();
    assert_eq!(artists.iter().filter(|a| **a == "Same").count(), 3);
    assert!(artists.contains(&"Other"));
}

#[test]
fn limit_and_duplicates_within_one_seed() {
    let related = rel(vec![("s1", t("s1", "A"), vec![t("a", "A"), t("a", "A"), t("b", "B"), t("c", "C")])]);
    let out = recommender::rank(&related, &[], 2, recommender::MAX_PER_ARTIST);
    assert_eq!(out.len(), 2);
    assert!((out[0]["score"].as_f64().unwrap() - 1.5).abs() < 1e-9);
}

#[tokio::test]
async fn recommend_survives_a_failing_seed() {
    let seeds = vec!["bad".to_string(), "good".to_string()];
    let out = recommender::recommend(&seeds, &[], 5, |seed| async move {
        if seed == "bad" {
            Err(Error::Net("offline".into()))
        } else {
            Ok((Some(t(&seed, "A")), vec![t("ok", "A")]))
        }
    })
    .await
    .unwrap();
    assert_eq!(out.iter().map(|r| r["video_id"].clone()).collect::<Vec<_>>(), vec![json!("ok")]);
}

#[tokio::test]
async fn recommend_raises_when_every_seed_fails() {
    let seeds = vec!["a".to_string(), "b".to_string()];
    let res =
        recommender::recommend(&seeds, &[], 30, |_| async { Err::<(Option<Value>, Vec<Value>), _>(Error::Net("offline".into())) }).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn recommend_without_seeds_is_empty() {
    let calls = Mutex::new(0);
    let out = recommender::recommend(&[], &["x".to_string()], 30, |_| {
        *calls.lock().unwrap() += 1;
        async { Ok((None, vec![])) }
    })
    .await
    .unwrap();
    assert!(out.is_empty());
    assert_eq!(*calls.lock().unwrap(), 0);
}

// ------------------------------------------------------------------ tagging

fn tempdir(tag: &str) -> std::path::PathBuf {
    let base = dirs_cache().join(format!("yt-test-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
    let base = std::path::PathBuf::from(base.to_string_lossy().replace(['(', ')'], ""));
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn dirs_cache() -> std::path::PathBuf {
    std::env::var_os("GEONIX_TEST_TMP").map(Into::into).unwrap_or_else(std::env::temp_dir)
}

#[test]
fn tags_mp3_with_cover() {
    let Some(ffmpeg) = geonix_yt::tools::locate("GEONIX_FFMPEG", "ffmpeg") else {
        eprintln!("ffmpeg missing; skipping");
        return;
    };
    let dir = tempdir("tag");
    let mp3 = dir.join("x.mp3");
    let st = std::process::Command::new(&ffmpeg)
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono", "-t", "1", "-q:a", "9"])
        .arg(&mp3)
        .status()
        .unwrap();
    assert!(st.success());
    let cover = b"\xff\xd8\xff\xe0fakejpeg".to_vec();
    downloader::tag_mp3(&mp3, "Tïtle", "Ärtist", "Album", Some(&cover)).unwrap();
    // Re-tagging replaces the cover instead of adding a second one.
    downloader::tag_mp3(&mp3, "Tïtle", "Ärtist", "", Some(&cover)).unwrap();
    let tag = id3::Tag::read_from_path(&mp3).unwrap();
    use id3::TagLike;
    assert_eq!(tag.title(), Some("Tïtle"));
    assert_eq!(tag.artist(), Some("Ärtist"));
    assert_eq!(tag.album(), Some("Album"));
    assert_eq!(tag.pictures().count(), 1);
    assert_eq!(tag.version(), id3::Version::Id3v23);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn audio_quality_setting_reaches_ytdlp() {
    let q = |args: Vec<String>| args[args.iter().position(|a| a == "--audio-quality").unwrap() + 1].clone();
    assert_eq!(q(downloader::ytdlp_args("abc", Path::new("/tmp/x"), 1, None)), "0");
    assert_eq!(q(downloader::ytdlp_args_with("abc", Path::new("/tmp/x"), 1, None, Some(192))), "192K");
    assert_eq!(q(downloader::ytdlp_args_with("abc", Path::new("/tmp/x"), 1, None, Some(320))), "320K");
    assert_eq!(downloader::audio_quality_arg(Some(9999)), "0");
}
