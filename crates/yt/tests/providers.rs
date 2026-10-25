//! Spotify / Apple Music / Deezer / SoundCloud links: recognising them, reading
//! the recorded pages and finding the songs on YouTube Music. Offline, except
//! the `--ignored` live tests at the end (run with `--ignored --nocapture`).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{fixture, Replay};
use geonix_yt::net::{BoxFut, Client, Error, Result, Transport};
use geonix_yt::providers::{self, Kind, Service};
use geonix_yt::{Yt, YtConfig};
use serde_json::{json, Value};

fn text(name: &str) -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/providers").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json_fixture(name: &str) -> Value {
    serde_json::from_str(&text(name)).unwrap()
}

// -------------------------------------------------------------------- links

fn link(url: &str) -> (Service, Kind, String) {
    let l = providers::parse_link(url).unwrap_or_else(|| panic!("{url} should be recognised"));
    (l.service, l.kind, l.url)
}

#[test]
fn spotify_links() {
    let pl = (Service::Spotify, Kind::Playlist, "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M".to_string());
    for url in [
        "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M?si=abc123",
        "https://open.spotify.com/intl-de/playlist/37i9dQZF1DXcBWIGoYBM5M",
        "https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M?utm_source=generator",
        "open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
        "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
        "  https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M  ",
    ] {
        assert_eq!(link(url), pl, "{url}");
    }
    assert_eq!(link("https://open.spotify.com/album/4aawyAB9vmqN3uQ7FjRGTy").1, Kind::Album);
    assert_eq!(link("https://open.spotify.com/intl-es/track/2sFepQNyl1WjpROruP8bJ0?si=x").1, Kind::Track);
    assert_eq!(link("spotify:track:2sFepQNyl1WjpROruP8bJ0").2, "https://open.spotify.com/track/2sFepQNyl1WjpROruP8bJ0");
    assert_eq!(link("https://spotify.link/AbCdEf").1, Kind::Unknown);
}

#[test]
fn apple_music_links() {
    assert_eq!(
        link("https://music.apple.com/us/playlist/top-100-global/pl.d25f5d1181894928af76c85c967f8f31?l=de"),
        (
            Service::AppleMusic,
            Kind::Playlist,
            "https://music.apple.com/us/playlist/top-100-global/pl.d25f5d1181894928af76c85c967f8f31".into()
        )
    );
    assert_eq!(
        link("https://music.apple.com/de/album/discovery/697194953"),
        (Service::AppleMusic, Kind::Album, "https://music.apple.com/de/album/discovery/697194953".into())
    );
    // A song opened from its album is a song link.
    assert_eq!(
        link("https://music.apple.com/us/album/one-more-time/697194953?i=697195462&uo=4"),
        (Service::AppleMusic, Kind::Track, "https://music.apple.com/us/album/one-more-time/697194953?i=697195462".into())
    );
    assert_eq!(
        link("https://music.apple.com/us/song/harder-better-faster-stronger/697195787"),
        (Service::AppleMusic, Kind::Track, "https://music.apple.com/us/song/harder-better-faster-stronger/697195787".into())
    );
}

#[test]
fn deezer_links() {
    for (url, kind) in [
        ("https://www.deezer.com/playlist/3155776842", Kind::Playlist),
        ("https://www.deezer.com/en/playlist/3155776842?utm_source=deezer", Kind::Playlist),
        ("deezer.com/de/album/302127", Kind::Album),
        ("https://www.deezer.com/us/track/3135556", Kind::Track),
        ("https://deezer.page.link/AbCdEf123", Kind::Unknown),
    ] {
        assert_eq!(link(url).1, kind, "{url}");
    }
    assert_eq!(link("https://www.deezer.com/en/playlist/3155776842").2, "https://www.deezer.com/playlist/3155776842");
}

#[test]
fn soundcloud_links() {
    assert_eq!(
        link("https://soundcloud.com/forss/sets/soulhack?si=abc&utm_source=clipboard"),
        (Service::SoundCloud, Kind::Playlist, "https://soundcloud.com/forss/sets/soulhack".into())
    );
    assert_eq!(
        link("https://m.soundcloud.com/forss/flickermood?utm_medium=text"),
        (Service::SoundCloud, Kind::Track, "https://soundcloud.com/forss/flickermood".into())
    );
    assert_eq!(link("https://on.soundcloud.com/AbCdE").1, Kind::Unknown);
}

#[test]
fn other_links_are_not_recognised() {
    for url in [
        "",
        "daft punk",
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://open.spotify.com/artist/45dkTj5sMRSjrmBSBeiHym",
        "https://open.spotify.com/",
        "https://open.spotify.com.evil.com/playlist/37i9dQZF1DXcBWIGoYBM5M",
        "https://music.apple.com/us/artist/daft-punk/5468295",
        "https://www.deezer.com/en/artist/27",
        "https://soundcloud.com/forss",
        "https://soundcloud.com/forss/sets",
        "https://soundcloud.com/discover",
        "spotify:artist:45dkTj5sMRSjrmBSBeiHym",
    ] {
        assert!(providers::parse_link(url).is_none(), "{url} should not be recognised");
    }
}

// ------------------------------------------------------------------ parsers

#[test]
fn spotify_embed_playlist() {
    let e = providers::spotify_from_embed(&text("spotify_playlist.html")).unwrap();
    assert_eq!(e.collection.title, "Today’s Top Hits");
    assert_eq!(e.collection.tracks.len(), 4);
    assert_eq!(e.collection.tracks[0].title, "Housewife");
    assert_eq!(e.collection.tracks[0].artists, ["Tate McRae"]);
    assert_eq!(e.collection.tracks[0].duration_sec, 208);
    assert_eq!(e.token.as_deref(), Some("TEST_TOKEN"));
    assert!(!e.capped);
}

#[test]
fn spotify_embed_album_splits_artists() {
    let e = providers::spotify_from_embed(&text("spotify_album.html")).unwrap();
    assert_eq!(e.collection.title, "Global Warming");
    assert_eq!(e.collection.tracks[0].title, "Global Warming (feat. Sensato)");
    assert_eq!(e.collection.tracks[0].artists, ["Pitbull", "Sensato"], "the non-breaking space after the comma is removed");
}

#[test]
fn spotify_embed_track() {
    let e = providers::spotify_from_embed(&text("spotify_track.html")).unwrap();
    assert!(e.collection.single);
    assert_eq!(
        (e.collection.tracks[0].title.as_str(), e.collection.tracks[0].artists.clone(), e.collection.tracks[0].duration_sec),
        ("Housewife", vec!["Tate McRae".to_string()], 208)
    );
}

#[test]
fn spotify_embed_with_100_songs_is_capped() {
    let e = providers::spotify_from_embed(&text("spotify_playlist_capped.html")).unwrap();
    assert_eq!(e.collection.tracks.len(), 100);
    assert!(e.capped, "100 songs means there may be more");
}

#[test]
fn spotify_private_or_missing_page() {
    let err = providers::spotify_from_embed(
        "<html><script id=\"__NEXT_DATA__\" type=\"application/json\">{\"props\":{\"pageProps\":{}}}</script></html>",
    )
    .unwrap_err();
    assert!(err.to_string().contains("private or doesn't exist"), "{err}");
    assert!(providers::spotify_from_embed("<html>no data</html>").is_err());
}

#[test]
fn spotify_api_page_skips_local_and_empty_entries() {
    let (tracks, count) = providers::spotify_api_page(&json_fixture("spotify_api_page.json"), true);
    assert_eq!(count, 5);
    assert_eq!(tracks.len(), 3, "the local file and the removed song are dropped");
    assert!(!tracks[0].title.is_empty() && !tracks[0].artists.is_empty() && tracks[0].duration_sec > 100);
}

#[test]
fn apple_playlist_and_album() {
    let pl = providers::apple_from_page(&text("apple_playlist.html"), None).unwrap();
    assert_eq!(pl.title, "Top 100: Global");
    assert_eq!(pl.tracks.len(), 3);
    assert_eq!((pl.tracks[0].title.as_str(), pl.tracks[0].duration_sec), ("Housewife", 208));
    assert_eq!(pl.tracks[1].artists, ["KAROL G, Judeline & rusowsky"]);
    let note = pl.note.expect("the page shows 3 of 100 songs");
    assert!(note.contains("first 3 of 100"), "{note}");

    let album = providers::apple_from_page(&text("apple_album.html"), None).unwrap();
    assert_eq!(album.title, "Discovery");
    assert_eq!(album.tracks[0].artists, ["Daft Punk"]);
}

#[test]
fn apple_song_picks_the_song_out_of_its_album_page() {
    let one = providers::apple_from_page(&text("apple_album.html"), Some("697195570")).unwrap();
    assert!(one.single);
    assert_eq!((one.tracks.len(), one.tracks[0].title.as_str(), one.tracks[0].duration_sec), (1, "Aerodynamic", 212));
    assert!(providers::apple_from_page(&text("apple_album.html"), Some("1")).is_err());
}

#[test]
fn deezer_track_objects() {
    let page = json_fixture("deezer_album_tracks.json");
    let items = page["data"].as_array().unwrap();
    let first = providers::deezer_track(&items[0], Some("Album Artist")).unwrap();
    assert_eq!((first.title.as_str(), first.artists.clone()), ("One More Time", vec!["Daft Punk".to_string()]));
    let no_artist = providers::deezer_track(&json!({"title": "X", "duration": 61}), Some("Album Artist")).unwrap();
    assert_eq!((no_artist.artists, no_artist.duration_sec), (vec!["Album Artist".to_string()], 61));
}

#[test]
fn soundcloud_songs_get_their_own_id_and_download_url() {
    let track = providers::soundcloud_track(&json_fixture("soundcloud_track.json")).unwrap();
    assert_eq!(
        (track["video_id"].clone(), track["title"].clone(), track["downloaded"].clone()),
        (json!("sc_293"), json!("Flickermood"), json!(false))
    );
    assert!(providers::soundcloud_track(&json!({"title": "No id"})).is_none());
    assert!(
        geonix_yt::ytm::is_soundcloud_id("sc_293")
            && !geonix_yt::ytm::is_soundcloud_id("sc_")
            && !geonix_yt::ytm::is_soundcloud_id("dQw4w9WgXcQ")
    );
    assert_eq!(geonix_yt::ytm::watch_url("sc_293"), "https://api.soundcloud.com/tracks/293");
    assert_eq!(geonix_yt::ytm::watch_url("dQw4w9WgXcQ"), "https://music.youtube.com/watch?v=dQw4w9WgXcQ");
    let args = geonix_yt::downloader::ytdlp_args_with("sc_293", std::path::Path::new("/tmp/x"), 2, None, None);
    assert_eq!(args.last().unwrap(), "https://api.soundcloud.com/tracks/293");
    assert!(!args.iter().any(|a| a.starts_with("youtube:")), "no YouTube client options for SoundCloud");
}

#[test]
fn soundcloud_source_prefers_track_and_artist_fields() {
    let s = providers::soundcloud_source(&json_fixture("soundcloud_track.json")).unwrap();
    assert_eq!((s.title.as_str(), s.artists.clone(), s.duration_sec), ("Flickermood", vec!["Forss".to_string()], 213));
    let s = providers::soundcloud_source(&json!({"title": "Raw upload", "uploader": "Somebody", "duration": 61.5})).unwrap();
    assert_eq!((s.title.as_str(), s.artists.clone()), ("Raw upload", vec!["Somebody".to_string()]));
    assert!(providers::soundcloud_source(&json!({"id": "1"})).is_none());
}

// ---------------------------------------------- whole command, recorded pages

/// Serves recorded pages / API answers by URL and YouTube Music searches from a fixture.
struct Fake {
    search: Replay,
    pages: Vec<(&'static str, String)>,
    json: Vec<(&'static str, Value)>,
}

impl Fake {
    fn new(pages: Vec<(&'static str, String)>, json: Vec<(&'static str, Value)>) -> Self {
        let f = fixture("search_bohemian");
        let mut search = Replay::default();
        search.add_innertube("search", f["requests"][0]["response"].clone());
        Self { search, pages, json }
    }
}

impl Transport for Fake {
    fn innertube<'a>(&'a self, endpoint: &'a str, client: Client, body: Value, params: &'a str) -> BoxFut<'a, Result<Value>> {
        self.search.innertube(endpoint, client, body, params)
    }

    fn get_json<'a>(&'a self, url: &'a str, _ua: &'a str, _timeout: Duration) -> BoxFut<'a, Result<Value>> {
        Box::pin(async move {
            self.json
                .iter()
                .find(|(u, _)| url.contains(u))
                .map(|(_, v)| v.clone())
                .ok_or_else(|| Error::Net(format!("no fixture for {url}")))
        })
    }

    fn get_bytes<'a>(&'a self, url: &'a str, _ua: &'a str, _timeout: Duration) -> BoxFut<'a, Result<Vec<u8>>> {
        Box::pin(async move {
            self.pages
                .iter()
                .find(|(u, _)| url.contains(u))
                .map(|(_, v)| v.clone().into_bytes())
                .ok_or_else(|| Error::Net(format!("no fixture for {url}")))
        })
    }
}

fn spotify_page(tracks: Value) -> String {
    let data =
        json!({"props": {"pageProps": {"state": {"data": {"entity": {"type": "playlist", "name": "Road trip", "trackList": tracks}}}}}});
    format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>")
}

fn spotify_track(title: &str, artist: &str, ms: i64) -> Value {
    json!({"uri": "spotify:track:x", "title": title, "subtitle": artist, "duration": ms})
}

#[tokio::test]
async fn spotify_playlist_is_matched_on_youtube_music() {
    let page = spotify_page(json!([
        spotify_track("Bohemian Rhapsody", "Queen", 355_000),
        spotify_track("A Song Nobody Ever Uploaded", "Nobody At All", 200_000),
        spotify_track("Bohemian Rhapsody - Remastered 2011", "Queen", 354_000),
    ]));
    let yt = Yt::with_transport(YtConfig::default(), Arc::new(Fake::new(vec![("open.spotify.com/embed/playlist/ABCD1234", page)], vec![])));
    let out = yt.handle("playlist_info", json!({"url": "https://open.spotify.com/playlist/ABCD1234?si=zz"}), |_| {}).await.unwrap();
    assert_eq!(out["title"], "Road trip");
    assert_eq!(out["single"], false);
    let ids: Vec<&str> = out["tracks"].as_array().unwrap().iter().map(|t| t["video_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["BSTsnWoslP4"], "matched songs in the shared track shape; the same video is kept once");
    assert_eq!(out["unmatched"], json!(["Nobody At All - A Song Nobody Ever Uploaded"]));
    assert_eq!(out["tracks"][0]["downloaded"], false);
}

#[tokio::test]
async fn deezer_track_is_a_single() {
    let track =
        json!({"id": 1, "title": "Bohemian Rhapsody", "duration": 355, "artist": {"name": "Queen"}, "contributors": [{"name": "Queen"}]});
    let yt = Yt::with_transport(YtConfig::default(), Arc::new(Fake::new(vec![], vec![("api.deezer.com/track/1", track)])));
    let out = yt.handle("playlist_info", json!({"url": "https://www.deezer.com/en/track/1"}), |_| {}).await.unwrap();
    assert_eq!((out["single"].clone(), out["tracks"][0]["video_id"].clone()), (json!(true), json!("BSTsnWoslP4")));
}

#[tokio::test]
async fn deezer_playlist_pages_through_next() {
    let item = |t: &str| json!({"title": t, "duration": 355, "artist": {"name": "Queen"}});
    let page1 = json!({"data": [item("Bohemian Rhapsody")], "next": "https://api.deezer.com/playlist/7/tracks?index=100"});
    let page2 = json!({"data": [item("Somebody To Love Nobody Has")]});
    let fake = Fake::new(
        vec![],
        vec![
            ("playlist/7/tracks?limit=100&index=0", page1),
            ("playlist/7/tracks?limit=100&index=100", page2),
            ("api.deezer.com/playlist/7", json!({"title": "Queen mix"})),
        ],
    );
    let yt = Yt::with_transport(YtConfig::default(), Arc::new(fake));
    let out = yt.handle("playlist_info", json!({"url": "https://www.deezer.com/playlist/7"}), |_| {}).await.unwrap();
    assert_eq!(out["title"], "Queen mix");
    assert_eq!(out["tracks"].as_array().unwrap().len(), 1);
    assert_eq!(out["unmatched"].as_array().unwrap().len(), 1, "both pages were read: 1 matched + 1 unmatched");
}

#[tokio::test]
async fn private_deezer_playlist_gives_a_clear_error() {
    let err_body = json!({"error": {"type": "DataException", "message": "no data", "code": 800}});
    let yt = Yt::with_transport(YtConfig::default(), Arc::new(Fake::new(vec![], vec![("api.deezer.com/playlist/9", err_body)])));
    let err = yt.handle("playlist_info", json!({"url": "https://www.deezer.com/playlist/9"}), |_| {}).await.unwrap_err();
    assert_eq!(err, "That Deezer playlist is private or doesn't exist");
}

#[tokio::test]
async fn offline_matching_reports_the_network_error() {
    let page = spotify_page(json!([spotify_track("Bohemian Rhapsody", "Queen", 355_000)]));
    let mut fake = Fake::new(vec![("open.spotify.com/embed/playlist/OFFLINE1", page)], vec![]);
    fake.search = Replay::default();
    let yt = Yt::with_transport(YtConfig::default(), Arc::new(fake));
    let err = yt.handle("playlist_info", json!({"url": "https://open.spotify.com/playlist/OFFLINE1"}), |_| {}).await.unwrap_err();
    assert!(err.starts_with("Network error"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn soundcloud_set_is_read_with_ytdlp() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("geonix-fake-sc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("yt-dlp");
    // The set listing is flat (urls only); each song is then looked up on its own.
    let script = r#"#!/bin/sh
case "$*" in
  *--help*) exit 0 ;;
  *--flat-playlist*) echo '{"title": "My set", "entries": [{"url": "https://soundcloud.com/q/bohemian"}, {"url": "https://soundcloud.com/q/broken"}]}' ;;
  *broken*) echo 'ERROR: [soundcloud] 404' >&2; exit 1 ;;
  *) echo '{"id": "77", "title": "Bohemian Rhapsody", "track": "Bohemian Rhapsody", "artist": "Queen", "duration": 355.2, "thumbnail": "https://i1.sndcdn.com/a.jpg"}' ;;
esac
"#;
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let yt = Yt::with_transport(YtConfig { ytdlp: Some(path), ..Default::default() }, Arc::new(Fake::new(vec![], vec![])));
    let out = yt.handle("playlist_info", json!({"url": "https://soundcloud.com/q/sets/my-set"}), |_| {}).await.unwrap();
    assert_eq!(out["title"], "My set");
    assert_eq!(out["tracks"][0]["video_id"], "sc_77", "SoundCloud songs are not matched on YouTube");
    assert_eq!(
        (out["tracks"][0]["artist"].clone(), out["tracks"][0]["thumbnail_url"].clone()),
        (json!("Queen"), json!("https://i1.sndcdn.com/a.jpg"))
    );
    assert_eq!(out["unmatched"], json!(["(couldn't read https://soundcloud.com/q/broken)"]));
    std::fs::remove_dir_all(dir).ok();
}

// --------------------------------------------------------------------- live

/// Run one real link end to end and print the numbers.
async fn live(url: &str, min_tracks: usize) {
    let started = std::time::Instant::now();
    let out =
        Yt::new(YtConfig::default()).handle("playlist_info", json!({ "url": url }), |_| {}).await.unwrap_or_else(|e| panic!("{url}: {e}"));
    let matched = out["tracks"].as_array().unwrap().len();
    let unmatched = out["unmatched"].as_array().unwrap();
    println!(
        "{url}\n  \"{}\": {matched} matched, {} unmatched, single={}, note={}, {:.1}s",
        out["title"],
        unmatched.len(),
        out["single"],
        out["note"],
        started.elapsed().as_secs_f64()
    );
    for u in unmatched.iter().take(8) {
        println!("    unmatched: {u}");
    }
    assert!(matched + unmatched.len() >= min_tracks, "{out}");
    assert!(matched * 10 >= (matched + unmatched.len()) * 7, "at least 70% should match");
}

#[tokio::test]
#[ignore]
async fn live_spotify() {
    live("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M", 40).await;
    live("https://open.spotify.com/playlist/6mtYuOxzl58vSGnEDtZ9uB", 300).await;
    live("https://open.spotify.com/album/4aawyAB9vmqN3uQ7FjRGTy", 10).await;
    live("https://open.spotify.com/track/2sFepQNyl1WjpROruP8bJ0", 1).await;
}

#[tokio::test]
#[ignore]
async fn live_apple_music() {
    live("https://music.apple.com/us/playlist/top-100-global/pl.d25f5d1181894928af76c85c967f8f31", 90).await;
    live("https://music.apple.com/us/album/discovery/697194953", 10).await;
    live("https://music.apple.com/us/album/one-more-time/697194953?i=697195462", 1).await;
}

#[tokio::test]
#[ignore]
async fn live_deezer() {
    live("https://www.deezer.com/playlist/3155776842", 50).await;
    live("https://www.deezer.com/album/302127", 10).await;
    live("https://www.deezer.com/track/3135556", 1).await;
}

#[tokio::test]
#[ignore]
async fn live_soundcloud() {
    live("https://soundcloud.com/forss/sets/soulhack", 8).await;
    live("https://soundcloud.com/forss/flickermood", 1).await;
}

#[cfg(unix)]
#[tokio::test]
async fn soundcloud_song_downloads_from_soundcloud_and_is_tagged() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("geonix-fake-scdl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("yt-dlp");
    let log = dir.join("args.txt");
    // Stands in for yt-dlp: records its arguments and "downloads" an MP3 where -o says.
    let script = format!(
        r#"#!/bin/sh
case "$*" in *--help*) exit 0 ;; esac
echo "$@" > {log}
for a in "$@"; do if [ "$prev" = "-o" ]; then out="$a"; fi; if [ "$prev" = "after_move:%()j" ]; then info="$a"; fi; prev="$a"; done
printf 'ID3' > "$(dirname "$out")/sc_293.mp3"
echo '{{"id": "293"}}' > "$info"
"#,
        log = log.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let yt = Yt::with_transport(YtConfig { ytdlp: Some(path), ..Default::default() }, Arc::new(Replay::default()));
    let meta = json!({"video_id": "sc_293", "title": "Flickermood", "artist": "Forss", "album": "", "duration_sec": 213});
    let music = dir.join("music");
    let out =
        yt.handle("download", json!({"video_id": "sc_293", "dest_dir": music.to_string_lossy(), "meta": meta}), |_| {}).await.unwrap();
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.trim_end().ends_with("-- https://api.soundcloud.com/tracks/293"), "{args}");
    assert!(!args.contains("youtube:"), "{args}");
    assert_eq!(
        (out["video_id"].clone(), out["title"].clone(), out["artist"].clone()),
        (json!("sc_293"), json!("Flickermood"), json!("Forss"))
    );
    let file = out["file_path"].as_str().unwrap();
    assert!(std::path::Path::new(file).exists() && file.contains("Forss"), "{file}");
    let tag = id3::Tag::read_from_path(file).unwrap();
    assert_eq!((id3::TagLike::title(&tag), id3::TagLike::artist(&tag)), (Some("Flickermood"), Some("Forss")));
    std::fs::remove_dir_all(dir).ok();
}

#[tokio::test]
async fn soundcloud_ids_do_not_disturb_recommendations_or_lyrics() {
    let yt = offline_yt();
    // Only SoundCloud seeds: nothing to ask YouTube Music, and no error.
    let out = yt.handle("recommend", json!({"seed_video_ids": ["sc_293", "sc_1"]}), |_| {}).await.unwrap();
    assert_eq!(out["tracks"], json!([]));
    // No YouTube Music lookup for the lyrics of a SoundCloud song (offline transport would fail it).
    let out = yt.handle("lyrics", json!({"video_id": "sc_293", "title": "", "artist": ""}), |_| {}).await.unwrap();
    assert_eq!(out["transient"], false, "{out}");
}

fn offline_yt() -> Yt {
    Yt::with_transport(YtConfig::default(), Arc::new(Replay::default()))
}
