//! Download queue: at most 3 downloads run at once. Every state change is
//! saved to the database and broadcast to `GET /api/downloads/events`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc, Notify, OwnedSemaphorePermit, Semaphore};

use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::models::{now_ms, Download, DownloadedTrack, PlaylistInfo, Track};
use crate::provider::Music;

/// Hard cap; the `parallel_downloads` setting (1..=4) holds back the rest.
pub const MAX_PARALLEL: usize = crate::settings::MAX_PARALLEL as usize;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const INFO_TIMEOUT: Duration = Duration::from_secs(120);
/// An import within this time after its preview reuses the preview's songs
/// instead of asking YouTube again.
const INFO_REUSE: Duration = Duration::from_secs(300);
/// At most this many progress saves/broadcasts per download.
const PROGRESS_EVERY: Duration = Duration::from_millis(300);

#[derive(Clone)]
pub struct DownloadManager {
    db: Arc<Db>,
    music: Music,
    permits: Arc<Semaphore>,
    /// Permits held back to lower the limit below MAX_PARALLEL.
    held: Arc<tokio::sync::Mutex<Vec<OwnedSemaphorePermit>>>,
    events: broadcast::Sender<Download>,
    music_dir: PathBuf,
    covers_dir: PathBuf,
    after: Option<AfterDownloads>,
    /// Recent playlist previews by URL (see INFO_REUSE).
    recent_info: Arc<std::sync::Mutex<HashMap<String, (Instant, PlaylistInfo)>>>,
}

#[derive(Debug, Serialize)]
pub struct PlaylistImport {
    /// `None` for a "songs only" import, which doesn't touch any playlist.
    pub playlist_id: Option<i64>,
    pub name: String,
    pub total: usize,
    pub queued: usize,
    /// Songs that were already downloaded (added without downloading).
    pub skipped: usize,
    /// Songs that were already in the target playlist (left untouched).
    pub already_in_playlist: usize,
    pub downloads: Vec<Download>,
}

/// Where an import puts the YouTube songs.
pub enum ImportTarget<'a> {
    /// Create a new playlist, named by the user or after the YouTube title.
    New { name: Option<&'a str> },
    /// Append to an existing playlist, keeping its name and current songs.
    Existing(i64),
    /// Only download the missing songs into the library; no playlist changes.
    LibraryOnly,
}

/// Runs a shell command (`GEONIX_AFTER_DOWNLOADS`) once things have settled
/// after downloads or imports, e.g. to export playlists and rescan the phone
/// server. Bursts are coalesced: it runs `QUIET` after the last poke.
#[derive(Clone)]
pub struct AfterDownloads {
    notify: Arc<Notify>,
}

impl AfterDownloads {
    const QUIET: Duration = Duration::from_secs(10);

    pub fn spawn(command: String) -> Self {
        let notify = Arc::new(Notify::new());
        let waiter = notify.clone();
        tokio::spawn(async move {
            loop {
                waiter.notified().await;
                // Wait until no new poke arrived for QUIET.
                while tokio::time::timeout(Self::QUIET, waiter.notified()).await.is_ok() {}
                let mut cmd = if cfg!(windows) {
                    let mut c = tokio::process::Command::new("cmd");
                    c.arg("/C");
                    c
                } else {
                    let mut c = tokio::process::Command::new("sh");
                    c.arg("-c");
                    c
                };
                match cmd.arg(&command).status().await {
                    Ok(st) if st.success() => tracing::info!("after-downloads hook finished"),
                    Ok(st) => tracing::warn!("after-downloads hook exited with {st}"),
                    Err(e) => tracing::warn!("after-downloads hook could not run: {e}"),
                }
            }
        });
        Self { notify }
    }

    pub fn poke(&self) {
        self.notify.notify_one();
    }
}

/// Result of placing imported songs into a playlist.
#[derive(Debug, Default, PartialEq)]
struct ImportPlan {
    to_download: Vec<String>,
    in_library: usize,
    already_in_playlist: usize,
}

impl DownloadManager {
    pub fn new(db: Arc<Db>, music: Music, music_dir: PathBuf, covers_dir: PathBuf, after: Option<AfterDownloads>) -> Self {
        let (events, _) = broadcast::channel(256);
        let mgr = Self {
            db,
            music,
            permits: Arc::new(Semaphore::new(MAX_PARALLEL)),
            held: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            events,
            music_dir,
            covers_dir,
            after,
            recent_info: Arc::default(),
        };
        // Default limit 3 until the settings say otherwise.
        if let Ok(p) = mgr.permits.clone().try_acquire_many_owned((MAX_PARALLEL - 3) as u32) {
            if let Ok(mut h) = mgr.held.try_lock() {
                h.push(p);
            }
        }
        mgr
    }

    /// Change how many downloads run at once (1..=MAX_PARALLEL). Lowering it
    /// lets running downloads finish; raising it starts queued ones at once.
    pub fn set_parallel(&self, n: usize) {
        let want_held = MAX_PARALLEL - n.clamp(1, MAX_PARALLEL);
        let permits = self.permits.clone();
        let held = self.held.clone();
        let adjust = async move {
            let mut h = held.lock().await;
            let mut have: usize = h.iter().map(|p| p.num_permits()).sum();
            while have > want_held {
                // Split off single permits to release exactly the surplus.
                let mut p = h.pop().unwrap();
                let n = p.num_permits();
                let keep = n.saturating_sub(have - want_held);
                if keep > 0 {
                    if let Some(rest) = p.split(keep) {
                        h.push(rest);
                    }
                }
                drop(p);
                have = h.iter().map(|p| p.num_permits()).sum();
            }
            while have < want_held {
                match permits.clone().acquire_owned().await {
                    Ok(p) => h.push(p),
                    Err(_) => return,
                }
                have += 1;
            }
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(adjust);
        }
    }

    #[cfg(test)]
    pub async fn parallel_limit(&self) -> usize {
        MAX_PARALLEL - self.held.lock().await.iter().map(|p| p.num_permits()).sum::<usize>()
    }

    /// Tell the after-downloads hook (if configured) that the library changed.
    pub fn library_changed(&self) {
        if let Some(after) = &self.after {
            after.poke();
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Download> {
        self.events.subscribe()
    }

    fn save(&self, d: &Download) {
        if let Err(e) = self.db.update_download(d) {
            tracing::error!("saving download {} failed: {e}", d.id);
        }
        let _ = self.events.send(d.clone());
    }

    /// Queue one song. Songs already in the library are reported as
    /// "skipped"; a song that is already queued returns the existing entry.
    pub fn enqueue(&self, video_id: &str, playlist_id: Option<i64>) -> ApiResult<Download> {
        if let Some(active) = self.db.active_download_for(video_id)? {
            return Ok(active);
        }
        let known = self.db.get_track(video_id)?;
        let now = now_ms();
        let mut d = Download {
            id: uuid::Uuid::new_v4().to_string(),
            video_id: video_id.to_string(),
            title: known.as_ref().map(|t| t.title.clone()).unwrap_or_default(),
            artist: known.as_ref().map(|t| t.artist.clone()).unwrap_or_default(),
            status: "queued".into(),
            progress: 0.0,
            speed: None,
            error: None,
            playlist_id,
            created_at: now,
            updated_at: now,
        };

        if has_file(known.as_ref()) {
            d.status = "skipped".into();
            d.progress = 100.0;
            self.db.insert_download(&d)?;
            let _ = self.events.send(d.clone());
            return Ok(d);
        }

        self.db.insert_download(&d)?;
        // A new attempt replaces earlier failures of the same song.
        self.db.delete_failed_for(video_id, &d.id)?;
        let _ = self.events.send(d.clone());
        self.start(d.clone(), known);
        Ok(d)
    }

    pub fn retry(&self, id: &str) -> ApiResult<Download> {
        let mut d = self.db.get_download(id)?.ok_or_else(|| ApiError::not_found("download not found"))?;
        if d.status != "failed" {
            return Err(ApiError::bad_request(format!("download is {}, only failed downloads can be retried", d.status)));
        }
        let known = self.db.get_track(&d.video_id)?;
        d.error = None;
        d.speed = None;
        d.updated_at = now_ms();
        self.db.delete_failed_for(&d.video_id, &d.id)?;
        if has_file(known.as_ref()) {
            // Downloaded in the meantime (e.g. by another attempt).
            d.status = "skipped".into();
            d.progress = 100.0;
            self.save(&d);
            return Ok(d);
        }
        d.status = "queued".into();
        d.progress = 0.0;
        self.save(&d);
        self.start(d.clone(), known);
        Ok(d)
    }

    fn start(&self, d: Download, known: Option<Track>) {
        let this = self.clone();
        tokio::spawn(async move { this.run(d, known).await });
    }

    async fn run(&self, mut d: Download, known: Option<Track>) {
        let Ok(_permit) = self.permits.clone().acquire_owned().await else {
            return;
        };
        d.status = "downloading".into();
        d.updated_at = now_ms();
        self.save(&d);

        // Forward progress events, throttled so the DB isn't hammered. The
        // latest update is always saved once the throttle window ends, so a
        // fast download doesn't sit at its first value (often 0 %) while the
        // MP3 conversion runs.
        let (ptx, mut prx) = mpsc::unbounded_channel::<crate::provider::Progress>();
        let progress_mgr = self.clone();
        let mut progress_dl = d.clone();
        let forwarder = tokio::spawn(async move {
            let mut last = Instant::now() - PROGRESS_EVERY;
            let mut pending = false;
            loop {
                let next = if pending {
                    let wait = (last + PROGRESS_EVERY).saturating_duration_since(Instant::now());
                    tokio::time::timeout(wait, prx.recv()).await.ok()
                } else {
                    Some(prx.recv().await)
                };
                match next {
                    Some(Some(p)) => {
                        progress_dl.progress = p.percent.clamp(0.0, 100.0);
                        progress_dl.speed = p.speed;
                        pending = true;
                    }
                    Some(None) => break,
                    // Throttle window over: save what we have.
                    None => {}
                }
                if pending && last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    pending = false;
                    progress_dl.updated_at = now_ms();
                    progress_mgr.save(&progress_dl);
                }
            }
        });

        let prefs = crate::settings::load(&self.db);
        let cmd = json!({
            "cmd": "download",
            "video_id": d.video_id,
            "dest_dir": prefs.download_dir_or(&self.music_dir),
            "audio_kbps": prefs.kbps(),
            "retry": prefs.auto_retry,
            "covers_dir": self.covers_dir,
            "meta": known,
        });
        let result = self.music.request_with_progress(cmd, Some(ptx), DOWNLOAD_TIMEOUT).await;
        forwarder.abort();
        // Wait until it really stopped, so a late progress save can't
        // overwrite the final state below.
        let _ = forwarder.await;

        d.updated_at = now_ms();
        d.speed = None;
        match result.and_then(|data| {
            serde_json::from_value::<DownloadedTrack>(data)
                .map_err(|e| ApiError::internal(format!("bad download result from provider: {e}")))
        }) {
            Ok(done) => {
                let file = done.track.file_path.clone().unwrap_or_default();
                match self.db.mark_downloaded(&done.track, &file, done.cover_path.as_deref()) {
                    Ok(()) => {
                        if let Err(e) = self.db.delete_failed_for(&d.video_id, &d.id) {
                            tracing::warn!("clearing old failures of {}: {e}", d.video_id);
                        }
                        d.status = "done".into();
                        d.progress = 100.0;
                        self.library_changed();
                        d.title = done.track.title.clone();
                        d.artist = done.track.artist.clone();
                        if let Some(pid) = d.playlist_id {
                            // Imported playlists already contain the song; this is a no-op then.
                            if let Err(e) = self.db.add_to_playlist(pid, &d.video_id) {
                                tracing::warn!("adding {} to playlist {pid}: {e}", d.video_id);
                            }
                        }
                    }
                    Err(e) => {
                        d.status = "failed".into();
                        d.error = Some(format!("saving to library failed: {e}"));
                    }
                }
            }
            Err(e) => {
                d.status = "failed".into();
                d.error = Some(e.to_string());
            }
        }
        self.save(&d);
    }

    /// Ask the provider what is in a playlist (used for the preview and import).
    pub async fn playlist_info(&self, url: &str) -> ApiResult<PlaylistInfo> {
        let data = self.music.request(json!({"cmd": "playlist_info", "url": url}), INFO_TIMEOUT).await?;
        let info: PlaylistInfo =
            serde_json::from_value(data).map_err(|e| ApiError::internal(format!("bad playlist info from provider: {e}")))?;
        for t in &info.tracks {
            self.db.upsert_track_meta(t)?;
        }
        if let Ok(mut recent) = self.recent_info.lock() {
            recent.retain(|_, (at, _)| at.elapsed() < INFO_REUSE);
            recent.insert(url.to_string(), (Instant::now(), info.clone()));
        }
        Ok(info)
    }

    /// The preview fetched for `url` within INFO_REUSE, else a fresh one.
    async fn playlist_info_for_import(&self, url: &str) -> ApiResult<PlaylistInfo> {
        let recent =
            self.recent_info.lock().ok().and_then(|r| r.get(url).filter(|(at, _)| at.elapsed() < INFO_REUSE).map(|(_, i)| i.clone()));
        match recent {
            Some(info) => Ok(info),
            None => self.playlist_info(url).await,
        }
    }

    /// Import a YouTube playlist into a new or an existing local playlist.
    /// Songs keep the YouTube order (appended after existing songs), songs
    /// already in the playlist are skipped, songs already downloaded are added
    /// right away, and only the missing ones are queued for download.
    pub async fn enqueue_playlist(&self, url: &str, target: ImportTarget<'_>) -> ApiResult<PlaylistImport> {
        if let ImportTarget::Existing(id) = target {
            self.check_import_target(id)?;
        }
        let info = self.playlist_info_for_import(url).await?;
        if info.tracks.is_empty() {
            return Err(ApiError::bad_request("that playlist has no available songs"));
        }
        let ids: Vec<String> = info.tracks.iter().map(|t| t.video_id.clone()).collect();
        let (playlist_id, name, plan) = match target {
            ImportTarget::New { name } => {
                let name = playlist_name(name, &info.title);
                let id = self.db.create_playlist(&name, "imported", Some(url))?;
                (Some(id), name, place_tracks(&self.db, id, &ids)?)
            }
            ImportTarget::Existing(id) => {
                let name = self.db.playlist(id)?.playlist.name;
                (Some(id), name, place_tracks(&self.db, id, &ids)?)
            }
            ImportTarget::LibraryOnly => (None, info.title.clone(), missing_tracks(&self.db, &ids)?),
        };

        let mut downloads = Vec::new();
        for id in &plan.to_download {
            downloads.push(self.enqueue(id, playlist_id)?);
        }
        self.library_changed();
        Ok(PlaylistImport {
            playlist_id,
            name,
            total: ids.len(),
            queued: downloads.len(),
            skipped: plan.in_library,
            already_in_playlist: plan.already_in_playlist,
            downloads,
        })
    }

    /// Existing playlists can receive imports, except the generated Discover Mix.
    fn check_import_target(&self, id: i64) -> ApiResult<()> {
        let playlist = match self.db.playlist(id) {
            Ok(p) => p.playlist,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Err(ApiError::not_found(format!("playlist {id} not found"))),
            Err(e) => return Err(e.into()),
        };
        if playlist.kind == "discover" {
            return Err(ApiError::bad_request("the Discover Mix is generated automatically; import into another playlist"));
        }
        Ok(())
    }
}

/// Append `ids` (in order) to a playlist, skipping songs it already has.
/// Returns which songs still need downloading.
fn place_tracks(db: &Db, playlist_id: i64, ids: &[String]) -> ApiResult<ImportPlan> {
    let mut seen: std::collections::HashSet<String> = db.playlist(playlist_id)?.tracks.into_iter().map(|t| t.video_id).collect();
    let mut plan = ImportPlan::default();
    for id in ids {
        if !seen.insert(id.clone()) {
            plan.already_in_playlist += 1;
            continue;
        }
        db.add_to_playlist(playlist_id, id)?;
        if has_file(db.get_track(id)?.as_ref()) {
            plan.in_library += 1;
        } else {
            plan.to_download.push(id.clone());
        }
    }
    Ok(plan)
}

/// For "songs only": which songs (in order, without duplicates) still need
/// downloading. No playlist is touched.
fn missing_tracks(db: &Db, ids: &[String]) -> ApiResult<ImportPlan> {
    let mut seen = std::collections::HashSet::new();
    let mut plan = ImportPlan::default();
    for id in ids {
        if !seen.insert(id.as_str()) {
            continue;
        }
        if has_file(db.get_track(id)?.as_ref()) {
            plan.in_library += 1;
        } else {
            plan.to_download.push(id.clone());
        }
    }
    Ok(plan)
}

/// The user's name for an imported playlist, or the YouTube title if blank.
fn playlist_name(custom: Option<&str>, youtube_title: &str) -> String {
    match custom.map(str::trim) {
        Some(n) if !n.is_empty() => n.chars().take(120).collect(),
        _ => youtube_title.to_string(),
    }
}

/// True when the song is in the library and its MP3 is still on disk.
fn has_file(track: Option<&Track>) -> bool {
    track.and_then(|t| t.file_path.as_ref()).map(|p| std::path::Path::new(p).exists()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Downloads fast (progress lines within a few ms), then converts for a while.
    struct FastThenConvert(PathBuf);

    impl crate::provider::Provider for FastThenConvert {
        fn handle<'a>(
            &'a self,
            _cmd: &'a str,
            _params: serde_json::Value,
            progress: crate::provider::ProgressFn,
        ) -> crate::provider::BoxFuture<'a, Result<serde_json::Value, String>> {
            Box::pin(async move {
                progress(json!({"percent": 0.0, "speed": "1MiB/s"}));
                progress(json!({"percent": 60.0, "speed": "9MiB/s"}));
                progress(json!({"percent": 95.0, "speed": "converting"}));
                tokio::time::sleep(Duration::from_millis(900)).await;
                let file = self.0.join("song.mp3");
                std::fs::write(&file, b"ID3").unwrap();
                Ok(json!({"video_id": "aaaaaaaaaaa", "title": "Song", "artist": "A", "file_path": file.to_string_lossy()}))
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn latest_progress_is_shown_while_converting() {
        let dir = std::env::temp_dir().join(format!("geonix-progress-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(Db::open_in_memory().unwrap());
        let mgr = DownloadManager::new(db, Music::new(FastThenConvert(dir.clone())), dir.clone(), dir.join("covers"), None);
        let mut events = mgr.subscribe();
        mgr.enqueue("aaaaaaaaaaa", None).unwrap();
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let Ok(Ok(d)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await else { break };
            let finished = d.status == "done" || d.status == "failed";
            seen.push((d.status, d.progress, d.speed));
            if finished {
                break;
            }
        }
        let converting = seen.iter().position(|(st, p, sp)| st == "downloading" && *p == 95.0 && sp.as_deref() == Some("converting"));
        assert!(converting.is_some(), "the conversion phase must reach the clients: {seen:?}");
        assert_eq!(seen.last().map(|(st, p, _)| (st.as_str(), *p)), Some(("done", 100.0)), "{seen:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn playlist_name_prefers_custom_name() {
        assert_eq!(playlist_name(Some("  Road trip "), "YT Title"), "Road trip");
        assert_eq!(playlist_name(Some("   "), "YT Title"), "YT Title");
        assert_eq!(playlist_name(None, "YT Title"), "YT Title");
        assert_eq!(playlist_name(Some(&"x".repeat(300)), "t").len(), 120);
    }

    #[test]
    fn has_file_requires_existing_mp3() {
        let dir = std::env::temp_dir().join(format!("geonix-hasfile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mp3 = dir.join("song.mp3");
        std::fs::write(&mp3, b"id3").unwrap();
        let mut t = Track { video_id: "a".into(), title: "A".into(), ..Default::default() };
        assert!(!has_file(None));
        assert!(!has_file(Some(&t)));
        t.file_path = Some(mp3.to_string_lossy().into_owned());
        assert!(has_file(Some(&t)));
        std::fs::remove_file(&mp3).unwrap();
        assert!(!has_file(Some(&t)), "a deleted file must be downloaded again");
        std::fs::remove_dir_all(dir).ok();
    }

    fn library_track(db: &Db, dir: &std::path::Path, id: &str) {
        let mp3 = dir.join(format!("{id}.mp3"));
        std::fs::write(&mp3, b"id3").unwrap();
        let t = Track { video_id: id.into(), title: id.into(), ..Default::default() };
        db.mark_downloaded(&t, &mp3.to_string_lossy(), None).unwrap();
    }

    fn remote_track(db: &Db, id: &str) {
        db.upsert_track_meta(&Track { video_id: id.into(), title: id.into(), ..Default::default() }).unwrap();
    }

    fn playlist_ids(db: &Db, id: i64) -> Vec<String> {
        db.playlist(id).unwrap().tracks.into_iter().map(|t| t.video_id).collect()
    }

    #[test]
    fn import_into_existing_playlist_appends_in_order_and_skips_present_songs() {
        let dir = std::env::temp_dir().join(format!("geonix-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_in_memory().unwrap();
        library_track(&db, &dir, "mine");
        library_track(&db, &dir, "have");
        for id in ["y1", "y3"] {
            remote_track(&db, id);
        }
        let p = db.create_playlist("smal", "user", None).unwrap();
        db.add_to_playlist(p, "mine").unwrap();

        // YouTube order: y1, mine (already in playlist), have (downloaded), y3
        let ids = ["y1", "mine", "have", "y3"].map(String::from);
        let plan = place_tracks(&db, p, &ids).unwrap();

        assert_eq!(playlist_ids(&db, p), ["mine", "y1", "have", "y3"]);
        assert_eq!(plan.already_in_playlist, 1);
        assert_eq!(plan.in_library, 1);
        assert_eq!(plan.to_download, ["y1", "y3"]);
        assert_eq!(db.playlist(p).unwrap().playlist.name, "smal");

        // Importing the same playlist again changes nothing.
        let again = place_tracks(&db, p, &ids).unwrap();
        assert_eq!(again.already_in_playlist, 4);
        assert!(again.to_download.is_empty());
        assert_eq!(playlist_ids(&db, p), ["mine", "y1", "have", "y3"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn songs_only_import_touches_no_playlist() {
        let dir = std::env::temp_dir().join(format!("geonix-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_in_memory().unwrap();
        library_track(&db, &dir, "have");
        remote_track(&db, "a");
        remote_track(&db, "c");
        let p = db.create_playlist("smal", "user", None).unwrap();
        db.add_to_playlist(p, "have").unwrap();
        let before = db.playlists().unwrap();

        let plan = missing_tracks(&db, &["a", "have", "c", "a"].map(String::from)).unwrap();
        assert_eq!(plan, ImportPlan { to_download: vec!["a".into(), "c".into()], in_library: 1, already_in_playlist: 0 });
        assert_eq!(db.playlists().unwrap(), before, "playlists must be unchanged");
        assert_eq!(playlist_ids(&db, p), ["have"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn songs_from_other_playlists_are_added_without_downloading_again() {
        let dir = std::env::temp_dir().join(format!("geonix-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_in_memory().unwrap();
        library_track(&db, &dir, "shared");
        remote_track(&db, "new");
        let other = db.create_playlist("big", "imported", None).unwrap();
        db.add_to_playlist(other, "shared").unwrap();

        let target = db.create_playlist("road trip", "user", None).unwrap();
        let plan = place_tracks(&db, target, &["shared", "new"].map(String::from)).unwrap();
        assert_eq!(playlist_ids(&db, target), ["shared", "new"]);
        assert_eq!(playlist_ids(&db, other), ["shared"], "the other playlist keeps the song");
        assert_eq!(plan, ImportPlan { to_download: vec!["new".into()], in_library: 1, already_in_playlist: 0 });
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn import_into_new_playlist_keeps_youtube_order() {
        let dir = std::env::temp_dir().join(format!("geonix-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_in_memory().unwrap();
        library_track(&db, &dir, "b");
        remote_track(&db, "a");
        remote_track(&db, "c");
        let p = db.create_playlist("Levels", "imported", None).unwrap();
        let plan = place_tracks(&db, p, &["a", "b", "c"].map(String::from)).unwrap();
        assert_eq!(playlist_ids(&db, p), ["a", "b", "c"]);
        assert_eq!(plan, ImportPlan { to_download: vec!["a".into(), "c".into()], in_library: 1, already_in_playlist: 0 });
        std::fs::remove_dir_all(dir).ok();
    }
}
