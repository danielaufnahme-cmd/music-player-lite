//! Hearted songs: songs starred in Navidrome (Amperfy hearts) go into a Lite
//! playlist, are unstarred, and the after-downloads hook exports the playlist
//! and rescans so it shows up on the phone.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{require_json, AppState};
use crate::error::{ApiError, ApiResult};
use crate::models::Track;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/favorites", get(list))
        .route("/api/favorites/to-playlist", post(to_playlist).route_layer(axum::middleware::from_fn(require_json)))
}

const NAVIDROME_TIMEOUT: Duration = Duration::from_secs(10);

/// How to reach Navidrome. The password is only used to sign requests.
struct Navidrome {
    url: String,
    user: String,
    password: String,
}

/// A song Navidrome has starred.
#[derive(Debug, Clone)]
struct Starred {
    id: String,
    title: String,
    artist: String,
    duration: i64,
    /// Path relative to Navidrome's music folder.
    path: String,
    starred: String,
}

impl Navidrome {
    /// Same login file and URL as `tools/navidrome_rescan.py`.
    fn from_env() -> ApiResult<Self> {
        let path = std::env::var_os("NAVIDROME_LOGIN_FILE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config/navidrome/LOGIN.txt"));
        let url = std::env::var("NAVIDROME_URL").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| "http://127.0.0.1:4533".into());
        Self::from_login_file(&path, &url)
    }

    fn from_login_file(path: &std::path::Path, url: &str) -> ApiResult<Self> {
        let not_set_up = || ApiError::status(axum::http::StatusCode::SERVICE_UNAVAILABLE, "Navidrome not set up (no login file)");
        let text = std::fs::read_to_string(path).map_err(|_| not_set_up())?;
        let field = |name: &str| {
            text.lines().filter_map(|l| l.split_once(':')).find(|(k, _)| *k == name).map(|(_, v)| v.trim().to_string()).unwrap_or_default()
        };
        let (user, password) = (field("Username"), field("Password"));
        if user.is_empty() || password.is_empty() {
            return Err(not_set_up());
        }
        Ok(Self { url: url.trim_end_matches('/').to_string(), user, password })
    }

    /// Subsonic token auth: t = md5(password + salt). Never logged.
    fn auth(&self) -> Vec<(&'static str, String)> {
        let mut raw = [0u8; 6];
        getrandom::getrandom(&mut raw).expect("the OS random number generator is unavailable");
        let salt: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let token = md5_hex(format!("{}{salt}", self.password).as_bytes());
        vec![
            ("u", self.user.clone()),
            ("t", token),
            ("s", salt),
            ("v", "1.16.1".into()),
            ("c", "music-player-lite".into()),
            ("f", "json".into()),
        ]
    }

    /// One Subsonic call. Errors never contain the request URL (it holds the token).
    async fn call(&self, http: &reqwest::Client, endpoint: &str, extra: &[(&str, String)]) -> ApiResult<Value> {
        let down = || ApiError::status(axum::http::StatusCode::BAD_GATEWAY, format!("Navidrome is not reachable at {}", self.url));
        let mut query = self.auth();
        query.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
        let res =
            http.get(format!("{}/rest/{endpoint}", self.url)).query(&query).timeout(NAVIDROME_TIMEOUT).send().await.map_err(|_| down())?;
        if !res.status().is_success() {
            return Err(ApiError::status(axum::http::StatusCode::BAD_GATEWAY, format!("Navidrome answered {}", res.status())));
        }
        let body: Value =
            res.json().await.map_err(|_| ApiError::status(axum::http::StatusCode::BAD_GATEWAY, "Navidrome sent an unreadable answer"))?;
        let body = body.get("subsonic-response").cloned().unwrap_or(Value::Null);
        if body["status"] != "ok" {
            let msg = body["error"]["message"].as_str().unwrap_or("request failed");
            return Err(ApiError::status(axum::http::StatusCode::BAD_GATEWAY, format!("Navidrome: {msg}")));
        }
        Ok(body)
    }

    /// Starred songs, oldest heart first.
    async fn starred(&self, http: &reqwest::Client) -> ApiResult<Vec<Starred>> {
        let body = self.call(http, "getStarred2", &[]).await?;
        let mut songs: Vec<Starred> = body["starred2"]["song"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| {
                        Some(Starred {
                            id: s["id"].as_str()?.to_string(),
                            title: s["title"].as_str().unwrap_or_default().to_string(),
                            artist: s["artist"].as_str().unwrap_or_default().to_string(),
                            duration: s["duration"].as_i64().unwrap_or(0),
                            path: s["path"].as_str().unwrap_or_default().to_string(),
                            starred: s["starred"].as_str().unwrap_or_default().to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        songs.sort_by(|a, b| a.starred.cmp(&b.starred)); // stable: equal stamps keep Navidrome's order
        Ok(songs)
    }

    async fn unstar(&self, http: &reqwest::Client, ids: &[String]) -> ApiResult<()> {
        let extra: Vec<(&str, String)> = ids.iter().map(|id| ("id", id.clone())).collect();
        self.call(http, "unstar", &extra).await.map(|_| ())
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The Lite song for a starred one: by file path (Navidrome's path is relative
/// to its music folder, Lite stores absolute paths), else by title + artist
/// (+ duration when both sides know it).
fn find_track<'a>(song: &Starred, library: &'a [Track]) -> Option<&'a Track> {
    let rel = song.path.replace('\\', "/");
    let rel = rel.trim_start_matches('/');
    if !rel.is_empty() {
        let tail = format!("/{rel}");
        let by_path = library.iter().find(|t| t.file_path.as_deref().is_some_and(|p| p.replace('\\', "/").ends_with(&tail)));
        if by_path.is_some() {
            return by_path;
        }
    }
    let (title, artist) = (norm(&song.title), norm(&song.artist));
    if title.is_empty() {
        return None;
    }
    library.iter().find(|t| {
        norm(&t.title) == title
            && norm(&t.artist) == artist
            && (song.duration <= 0 || t.duration_sec <= 0 || (song.duration - t.duration_sec).abs() <= 2)
    })
}

fn describe(s: &Starred) -> Value {
    json!({ "title": s.title, "artist": s.artist })
}

/// Hearted songs and whether Lite has each one.
async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let nd = Navidrome::from_env()?;
    list_with(&s, &nd).await.map(Json)
}

async fn list_with(s: &AppState, nd: &Navidrome) -> ApiResult<Value> {
    let starred = nd.starred(&s.http).await?;
    let library = s.db.library_tracks(None, None)?;
    let songs: Vec<Value> = starred
        .iter()
        .map(|st| json!({ "title": st.title, "artist": st.artist, "in_library": find_track(st, &library).is_some() }))
        .collect();
    Ok(json!({ "count": songs.len(), "songs": songs }))
}

#[derive(Deserialize)]
pub struct ToPlaylist {
    playlist_id: Option<i64>,
    new_name: Option<String>,
}

async fn to_playlist(State(s): State<AppState>, Json(body): Json<ToPlaylist>) -> ApiResult<Json<Value>> {
    let nd = Navidrome::from_env()?;
    to_playlist_with(&s, &nd, body).await.map(Json)
}

/// Add every hearted song Lite knows to the playlist (new or existing), then
/// unstar exactly those. Songs Lite can't map stay starred and are listed.
async fn to_playlist_with(s: &AppState, nd: &Navidrome, body: ToPlaylist) -> ApiResult<Value> {
    // Check the target before anything changes in Navidrome.
    let new_name = match (&body.playlist_id, body.new_name.as_deref().map(str::trim)) {
        (Some(_), Some(n)) if !n.is_empty() => return Err(ApiError::bad_request("send either playlist_id or new_name, not both")),
        (None, None | Some("")) => return Err(ApiError::bad_request("playlist_id or new_name is required")),
        (None, Some(n)) if n.chars().count() > 120 => return Err(ApiError::bad_request("playlist name is too long (max 120 characters)")),
        (None, Some(n)) => Some(n.to_string()),
        (Some(_), _) => None,
    };
    if let Some(id) = body.playlist_id {
        match s.db.playlist(id) {
            Ok(_) => {}
            Err(rusqlite::Error::QueryReturnedNoRows) => return Err(ApiError::not_found("playlist not found")),
            Err(e) => return Err(e.into()),
        }
    }

    let starred = nd.starred(&s.http).await?;
    let library = s.db.library_tracks(None, None)?;
    let mut mapped: Vec<(&Starred, &Track)> = Vec::new();
    let mut not_found = Vec::new();
    for st in &starred {
        match find_track(st, &library) {
            Some(t) => mapped.push((st, t)),
            None => not_found.push(describe(st)),
        }
    }

    // A new playlist is only made when there is something to put in it.
    let (playlist_id, created) = match (body.playlist_id, new_name) {
        (Some(id), _) => (Some(id), false),
        (None, Some(name)) if !mapped.is_empty() => (Some(s.db.create_playlist(&name, "user", None)?), true),
        _ => (None, false),
    };

    let (mut added, mut already) = (0, 0);
    let mut playlist_name = Value::Null;
    if let Some(id) = playlist_id {
        let mut present: HashSet<String> = s.db.playlist(id)?.tracks.into_iter().map(|t| t.video_id).collect();
        for (_, t) in &mapped {
            if present.insert(t.video_id.clone()) {
                s.db.add_to_playlist(id, &t.video_id)?;
                added += 1;
            } else {
                already += 1;
            }
        }
        playlist_name = json!(s.db.playlist(id)?.playlist.name);
    }

    // Everything mapped is now in the playlist, so unstar it; the rest stays hearted.
    let mut unstarred = 0;
    let mut unstar_error = Value::Null;
    if playlist_id.is_some() && !mapped.is_empty() {
        let ids: Vec<String> = mapped.iter().map(|(st, _)| st.id.clone()).collect();
        match nd.unstar(&s.http, &ids).await {
            Ok(()) => unstarred = ids.len(),
            Err(e) => unstar_error = json!(format!("Songs were added but could not be unstarred: {e}")),
        }
    }
    if added > 0 {
        s.downloads.library_changed();
    }

    Ok(json!({
        "playlist_id": playlist_id,
        "playlist_name": playlist_name,
        "created": created,
        "added": added,
        "already_there": already,
        "not_found": not_found,
        "unstarred": unstarred,
        "unstar_error": unstar_error,
    }))
}

/// MD5 as lowercase hex. Subsonic token auth requires it; this avoids a
/// dependency for one tiny use (not used for anything security-sensitive).
fn md5_hex(input: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = input.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0].iter().flat_map(|v| v.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::downloads::DownloadManager;
    use crate::provider::mock::MockProvider;
    use crate::provider::Music;
    use std::sync::{Arc, Mutex};

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    /// Fake Subsonic server: serves `starred` and records unstar calls.
    struct Fake {
        url: String,
        unstarred: Arc<Mutex<Vec<String>>>,
        starred: Arc<Mutex<Value>>,
    }

    async fn fake_navidrome(songs: Value) -> Fake {
        use axum::extract::RawQuery;
        let unstarred = Arc::new(Mutex::new(Vec::<String>::new()));
        let starred = Arc::new(Mutex::new(songs));
        let (u, st) = (unstarred.clone(), starred.clone());
        let app = Router::new()
            .route(
                "/rest/getStarred2",
                get(move || {
                    let st = st.clone();
                    async move { Json(json!({"subsonic-response": {"status": "ok", "starred2": {"song": st.lock().unwrap().clone()}}})) }
                }),
            )
            .route(
                "/rest/unstar",
                get(move |RawQuery(q): RawQuery| {
                    let u = u.clone();
                    async move {
                        let q = q.unwrap_or_default();
                        assert!(q.contains("t=") && q.contains("s=") && !q.contains("p="), "token auth expected");
                        u.lock().unwrap().extend(q.split('&').filter_map(|kv| kv.strip_prefix("id=")).map(String::from));
                        Json(json!({"subsonic-response": {"status": "ok"}}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { url, unstarred, starred }
    }

    fn nd(url: &str) -> Navidrome {
        Navidrome { url: url.into(), user: "me".into(), password: "secret".into() }
    }

    fn state() -> AppState {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let music = Music::new(MockProvider::offline());
        let dir = std::env::temp_dir();
        AppState {
            downloads: DownloadManager::new(db.clone(), music.clone(), dir.clone(), dir.clone(), None),
            sync: crate::sync::SyncManager::new(
                db.clone(),
                dir.clone(),
                DownloadManager::new(db.clone(), music.clone(), dir.clone(), dir.clone(), None),
                0,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))
            .unwrap(),
            db,
            music,
            http: reqwest::Client::new(),
            covers_dir: dir,
            stream_cache: Default::default(),
            recs_lock: Default::default(),
            sync_listener: Arc::new(crate::settings::SyncListener::new(None)),
        }
    }

    fn add_song(s: &AppState, id: &str, title: &str, artist: &str, file: &str) {
        let t = Track { video_id: id.into(), title: title.into(), artist: artist.into(), duration_sec: 100, ..Default::default() };
        s.db.mark_downloaded(&t, file, None).unwrap();
    }

    fn star(id: &str, title: &str, artist: &str, path: &str, at: &str) -> Value {
        json!({"id": id, "title": title, "artist": artist, "duration": 100, "path": path, "starred": at})
    }

    fn library(s: &AppState) {
        add_song(s, "aaaaaaaaaaa", "One", "Ann", "/music/Ann/One.mp3");
        add_song(s, "bbbbbbbbbbb", "Two", "Bob", "/music/Bob/Two.mp3");
        add_song(s, "ccccccccccc", "Three", "Cy", "/music/Cy/Three.mp3");
    }

    fn new_body(name: &str) -> ToPlaylist {
        ToPlaylist { playlist_id: None, new_name: Some(name.into()) }
    }

    #[tokio::test]
    async fn new_playlist_gets_mapped_songs_in_star_order_and_they_are_unstarred() {
        let s = state();
        library(&s);
        let fake = fake_navidrome(json!([
            star("n2", "Two", "Bob", "Bob/Two.mp3", "2026-02-01"),
            star("n1", "One", "Ann", "Ann/One.mp3", "2026-01-01"),
            // path unknown to Lite, found by title + artist
            star("n3", "three", "CY ", "Elsewhere/3.mp3", "2026-03-01"),
        ]))
        .await;
        let out = to_playlist_with(&s, &nd(&fake.url), new_body("Favorites 2026-10-25")).await.unwrap();
        assert_eq!((out["added"].as_i64(), out["already_there"].as_i64(), out["created"].as_bool()), (Some(3), Some(0), Some(true)));
        let id = out["playlist_id"].as_i64().unwrap();
        let ids: Vec<String> = s.db.playlist(id).unwrap().tracks.into_iter().map(|t| t.video_id).collect();
        assert_eq!(ids, ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]);
        assert_eq!(*fake.unstarred.lock().unwrap(), ["n1", "n2", "n3"]);
        assert_eq!(out["unstarred"], 3);
    }

    #[tokio::test]
    async fn existing_playlist_skips_songs_already_in_it() {
        let s = state();
        library(&s);
        let pid = s.db.create_playlist("Mine", "user", None).unwrap();
        s.db.add_to_playlist(pid, "aaaaaaaaaaa").unwrap();
        let fake =
            fake_navidrome(json!([star("n1", "One", "Ann", "Ann/One.mp3", "1"), star("n2", "Two", "Bob", "Bob/Two.mp3", "2")])).await;
        let out = to_playlist_with(&s, &nd(&fake.url), ToPlaylist { playlist_id: Some(pid), new_name: None }).await.unwrap();
        assert_eq!((out["added"].as_i64(), out["already_there"].as_i64(), out["created"].as_bool()), (Some(1), Some(1), Some(false)));
        assert_eq!(s.db.playlist(pid).unwrap().tracks.len(), 2);
        // The one already there is safe in the playlist, so it is unstarred too.
        assert_eq!(*fake.unstarred.lock().unwrap(), ["n1", "n2"]);
    }

    #[tokio::test]
    async fn unmappable_songs_stay_starred_and_are_listed() {
        let s = state();
        library(&s);
        let fake =
            fake_navidrome(json!([star("n1", "One", "Ann", "Ann/One.mp3", "1"), star("nx", "Ghost", "Nobody", "Nobody/Ghost.mp3", "2")]))
                .await;
        let out = to_playlist_with(&s, &nd(&fake.url), new_body("Fav")).await.unwrap();
        assert_eq!(out["added"], 1);
        assert_eq!(out["not_found"], json!([{"title": "Ghost", "artist": "Nobody"}]));
        assert_eq!(*fake.unstarred.lock().unwrap(), ["n1"]);

        // Nothing mappable: no empty playlist is created and nothing is unstarred.
        *fake.starred.lock().unwrap() = json!([star("nx", "Ghost", "Nobody", "Nobody/Ghost.mp3", "2")]);
        fake.unstarred.lock().unwrap().clear();
        let before = s.db.playlists().unwrap().len();
        let out = to_playlist_with(&s, &nd(&fake.url), new_body("Empty")).await.unwrap();
        assert_eq!((out["added"].as_i64(), out["playlist_id"].is_null()), (Some(0), true));
        assert_eq!(s.db.playlists().unwrap().len(), before);
        assert!(fake.unstarred.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_counts_hearts_and_flags_unknown_songs() {
        let s = state();
        library(&s);
        let fake = fake_navidrome(json!([star("n1", "One", "Ann", "Ann/One.mp3", "1"), star("nx", "Ghost", "Nobody", "x.mp3", "2")])).await;
        let out = list_with(&s, &nd(&fake.url)).await.unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["songs"][0]["in_library"], true);
        assert_eq!(out["songs"][1]["in_library"], false);
    }

    #[tokio::test]
    async fn bad_requests_and_missing_playlist_do_not_touch_navidrome() {
        let s = state();
        library(&s);
        let fake = fake_navidrome(json!([star("n1", "One", "Ann", "Ann/One.mp3", "1")])).await;
        let n = nd(&fake.url);
        for body in [
            ToPlaylist { playlist_id: None, new_name: None },
            ToPlaylist { playlist_id: None, new_name: Some("  ".into()) },
            ToPlaylist { playlist_id: Some(1), new_name: Some("x".into()) },
            ToPlaylist { playlist_id: Some(999), new_name: None },
        ] {
            assert!(to_playlist_with(&s, &n, body).await.is_err());
        }
        assert!(fake.unstarred.lock().unwrap().is_empty());
        assert!(s.db.playlists().unwrap().is_empty());
    }

    #[tokio::test]
    async fn navidrome_down_or_not_set_up_gives_clear_errors_and_changes_nothing() {
        let s = state();
        library(&s);
        // Nothing listens on this port.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        let err = to_playlist_with(&s, &nd(&url), new_body("Fav")).await.unwrap_err().to_string();
        assert!(err.contains("not reachable"), "{err}");
        assert!(!err.contains("secret") && !err.contains("t="), "error must not leak credentials: {err}");
        assert!(s.db.playlists().unwrap().is_empty());

        let missing = Navidrome::from_login_file(std::path::Path::new("/nonexistent/LOGIN.txt"), "http://x").err().unwrap();
        assert!(missing.to_string().contains("not set up"));
        let dir = std::env::temp_dir().join(format!("geonix-fav-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("LOGIN.txt");
        std::fs::write(&f, "Username: me\nPassword:\n").unwrap();
        assert!(Navidrome::from_login_file(&f, "http://x").is_err());
        std::fs::write(&f, "URL: http://x\nUsername: me\nPassword: pw: with colon\n").unwrap();
        let ok = Navidrome::from_login_file(&f, "http://x/").unwrap();
        assert_eq!((ok.user.as_str(), ok.password.as_str(), ok.url.as_str()), ("me", "pw: with colon", "http://x"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn failed_unstar_is_reported_but_playlist_keeps_the_songs() {
        let s = state();
        library(&s);
        let app = Router::new()
            .route("/rest/getStarred2", get(|| async { Json(json!({"subsonic-response": {"status": "ok", "starred2": {"song": [star("n1", "One", "Ann", "Ann/One.mp3", "1")]}}})) }))
            .route("/rest/unstar", get(|| async { Json(json!({"subsonic-response": {"status": "failed", "error": {"code": 0, "message": "boom"}}})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let out = to_playlist_with(&s, &nd(&url), new_body("Fav")).await.unwrap();
        assert_eq!((out["added"].as_i64(), out["unstarred"].as_i64()), (Some(1), Some(0)));
        assert!(out["unstar_error"].as_str().unwrap().contains("boom"));
    }
}
