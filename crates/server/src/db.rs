//! SQLite storage: schema migrations and all queries.
//!
//! The `tracks` table holds every song the app has seen (search results,
//! playlist imports, recommendations). A song is part of the *library* only
//! once it has a `file_path`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::models::{now_ms, Album, Artist, Download, Playlist, PlaylistDetail, Recommendation, Track};
use crate::podcasts::{Episode, FeedEpisode, Podcast};

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE tracks (
        video_id      TEXT PRIMARY KEY,
        title         TEXT NOT NULL,
        artist        TEXT NOT NULL DEFAULT '',
        album         TEXT NOT NULL DEFAULT '',
        duration_sec  INTEGER NOT NULL DEFAULT 0,
        thumbnail_url TEXT NOT NULL DEFAULT '',
        file_path     TEXT,
        cover_path    TEXT,
        play_count    INTEGER NOT NULL DEFAULT 0,
        last_played   INTEGER,
        added_at      INTEGER
    );
    CREATE INDEX idx_tracks_library ON tracks(file_path) WHERE file_path IS NOT NULL;

    CREATE TABLE playlists (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        name       TEXT NOT NULL,
        kind       TEXT NOT NULL DEFAULT 'user',
        source_url TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE playlist_tracks (
        playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
        video_id    TEXT NOT NULL REFERENCES tracks(video_id) ON DELETE CASCADE,
        position    INTEGER NOT NULL,
        PRIMARY KEY (playlist_id, video_id)
    );
    CREATE INDEX idx_playlist_tracks_pos ON playlist_tracks(playlist_id, position);

    CREATE TABLE downloads (
        id          TEXT PRIMARY KEY,
        video_id    TEXT NOT NULL,
        title       TEXT NOT NULL DEFAULT '',
        artist      TEXT NOT NULL DEFAULT '',
        status      TEXT NOT NULL,
        progress    REAL NOT NULL DEFAULT 0,
        speed       TEXT,
        error       TEXT,
        playlist_id INTEGER,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL
    );

    CREATE TABLE plays (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        video_id  TEXT NOT NULL REFERENCES tracks(video_id) ON DELETE CASCADE,
        played_at INTEGER NOT NULL
    );

    CREATE TABLE recommendations (
        video_id         TEXT PRIMARY KEY REFERENCES tracks(video_id) ON DELETE CASCADE,
        rank             INTEGER NOT NULL,
        score            REAL NOT NULL,
        because_video_id TEXT,
        because_artist   TEXT
    );

    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 2: lyrics cache (JSON from the provider; misses are cached too)
    r#"
    CREATE TABLE lyrics (
        video_id   TEXT PRIMARY KEY,
        json       TEXT NOT NULL,
        found      INTEGER NOT NULL,
        fetched_at INTEGER NOT NULL
    );
    "#,
    // 3: paired devices for backend-to-backend sync (settings live in `meta`)
    r#"
    CREATE TABLE IF NOT EXISTS peers (
        id           TEXT PRIMARY KEY,
        name         TEXT NOT NULL,
        address      TEXT NOT NULL,
        dns_name     TEXT,
        token        TEXT NOT NULL,
        last_sync_at INTEGER,
        last_error   TEXT,
        created_at   INTEGER NOT NULL
    );
    "#,
    // 4: podcasts (subscriptions and their episodes; files live outside the music folder)
    r#"
    CREATE TABLE IF NOT EXISTS podcasts (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        feed_url      TEXT NOT NULL UNIQUE,
        title         TEXT NOT NULL DEFAULT '',
        author        TEXT NOT NULL DEFAULT '',
        image_url     TEXT NOT NULL DEFAULT '',
        subscribed    INTEGER NOT NULL DEFAULT 1,
        subscribed_at INTEGER NOT NULL,
        last_checked  INTEGER,
        last_error    TEXT
    );

    CREATE TABLE IF NOT EXISTS podcast_episodes (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        podcast_id    INTEGER NOT NULL REFERENCES podcasts(id) ON DELETE CASCADE,
        guid          TEXT NOT NULL,
        title         TEXT NOT NULL DEFAULT '',
        published_at  INTEGER NOT NULL DEFAULT 0,
        duration_sec  INTEGER NOT NULL DEFAULT 0,
        enclosure_url TEXT NOT NULL,
        file_path     TEXT,
        status        TEXT NOT NULL DEFAULT 'none',
        progress      REAL NOT NULL DEFAULT 0,
        error         TEXT,
        position_sec  REAL NOT NULL DEFAULT 0,
        played        INTEGER NOT NULL DEFAULT 0,
        no_auto       INTEGER NOT NULL DEFAULT 0,
        UNIQUE (podcast_id, guid)
    );
    CREATE INDEX IF NOT EXISTS idx_podcast_episodes_pub ON podcast_episodes(podcast_id, published_at DESC);
    "#,
];

const TRACK_COLS: &str = "video_id, title, artist, album, duration_sec, thumbnail_url, \
                          file_path, play_count, last_played, added_at";

fn track_from_row(r: &Row) -> rusqlite::Result<Track> {
    let file_path: Option<String> = r.get(6)?;
    Ok(Track {
        video_id: r.get(0)?,
        title: r.get(1)?,
        artist: r.get(2)?,
        album: r.get(3)?,
        duration_sec: r.get(4)?,
        thumbnail_url: r.get(5)?,
        downloaded: file_path.is_some(),
        file_path,
        play_count: r.get(7)?,
        last_played: r.get(8)?,
        added_at: r.get(9)?,
    })
}

const DOWNLOAD_COLS: &str = "id, video_id, title, artist, status, progress, speed, error, playlist_id, created_at, updated_at";

fn download_from_row(r: &Row) -> rusqlite::Result<Download> {
    Ok(Download {
        id: r.get(0)?,
        video_id: r.get(1)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        status: r.get(4)?,
        progress: r.get(5)?,
        speed: r.get(6)?,
        error: r.get(7)?,
        playlist_id: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

/// Sort orders accepted by `GET /api/tracks?sort=`.
pub fn sort_clause(sort: Option<&str>) -> &'static str {
    match sort.unwrap_or("added") {
        "title" => "title COLLATE NOCASE ASC",
        "artist" => "artist COLLATE NOCASE ASC, album COLLATE NOCASE ASC, title COLLATE NOCASE ASC",
        "album" => "album COLLATE NOCASE ASC, title COLLATE NOCASE ASC",
        "plays" | "most_played" => "play_count DESC, last_played DESC",
        "recent" | "last_played" => "last_played IS NULL, last_played DESC",
        "duration" => "duration_sec DESC",
        _ => "added_at DESC, rowid DESC",
    }
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // Lite: small page cache (KiB when negative), no mmap, temp data on disk.
        conn.pragma_update(None, "cache_size", -1024)?;
        conn.pragma_update(None, "mmap_size", 0)?;
        conn.pragma_update(None, "temp_store", "FILE")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves the connection itself usable.
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let version: usize = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
            tracing::info!("applied database migration {}", i + 1);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- tracks

    /// Insert or refresh a song's metadata without touching library state
    /// (file, play count, date added).
    pub fn upsert_track_meta(&self, t: &Track) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO tracks (video_id, title, artist, album, duration_sec, thumbnail_url)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(video_id) DO UPDATE SET
                title = excluded.title,
                artist = excluded.artist,
                album = CASE WHEN excluded.album != '' THEN excluded.album ELSE tracks.album END,
                duration_sec = CASE WHEN excluded.duration_sec > 0 THEN excluded.duration_sec ELSE tracks.duration_sec END,
                thumbnail_url = CASE WHEN excluded.thumbnail_url != '' THEN excluded.thumbnail_url ELSE tracks.thumbnail_url END
             WHERE tracks.file_path IS NULL",
            params![t.video_id, t.title, t.artist, t.album, t.duration_sec, t.thumbnail_url],
        )?;
        Ok(())
    }

    /// Store a finished download: the song joins the library.
    pub fn mark_downloaded(&self, t: &Track, file_path: &str, cover_path: Option<&str>) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO tracks (video_id, title, artist, album, duration_sec, thumbnail_url, file_path, cover_path, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(video_id) DO UPDATE SET
                title = excluded.title, artist = excluded.artist, album = excluded.album,
                duration_sec = excluded.duration_sec, thumbnail_url = excluded.thumbnail_url,
                file_path = excluded.file_path, cover_path = excluded.cover_path,
                added_at = COALESCE(tracks.added_at, excluded.added_at)",
            params![t.video_id, t.title, t.artist, t.album, t.duration_sec, t.thumbnail_url, file_path, cover_path, now_ms()],
        )?;
        Ok(())
    }

    pub fn get_track(&self, video_id: &str) -> rusqlite::Result<Option<Track>> {
        self.conn().query_row(&format!("SELECT {TRACK_COLS} FROM tracks WHERE video_id = ?1"), [video_id], track_from_row).optional()
    }

    pub fn cover_path(&self, video_id: &str) -> rusqlite::Result<Option<(Option<String>, String)>> {
        self.conn()
            .query_row("SELECT cover_path, thumbnail_url FROM tracks WHERE video_id = ?1", [video_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
    }

    /// Library songs (downloaded only), filtered by a free-text query.
    pub fn library_tracks(&self, sort: Option<&str>, q: Option<&str>) -> rusqlite::Result<Vec<Track>> {
        let pattern = format!("%{}%", q.unwrap_or("").trim());
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TRACK_COLS} FROM tracks
             WHERE file_path IS NOT NULL
               AND (title LIKE ?1 OR artist LIKE ?1 OR album LIKE ?1)
             ORDER BY {}",
            sort_clause(sort)
        ))?;
        let rows = stmt.query_map([pattern], track_from_row)?;
        rows.collect()
    }

    pub fn library_ids(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT video_id FROM tracks WHERE file_path IS NOT NULL")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// Downloaded status for a batch of ids (used to annotate search results).
    pub fn downloaded_set(&self, ids: &[String]) -> rusqlite::Result<std::collections::HashSet<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT 1 FROM tracks WHERE video_id = ?1 AND file_path IS NOT NULL")?;
        let mut out = std::collections::HashSet::new();
        for id in ids {
            if stmt.exists([id])? {
                out.insert(id.clone());
            }
        }
        Ok(out)
    }

    pub fn record_play(&self, video_id: &str) -> rusqlite::Result<Option<Track>> {
        let now = now_ms();
        {
            let conn = self.conn();
            let n = conn
                .execute("UPDATE tracks SET play_count = play_count + 1, last_played = ?2 WHERE video_id = ?1", params![video_id, now])?;
            if n == 0 {
                return Ok(None);
            }
            conn.execute("INSERT INTO plays (video_id, played_at) VALUES (?1, ?2)", params![video_id, now])?;
        }
        self.get_track(video_id)
    }

    /// Remove a song from the library (and from every playlist).
    /// Returns the file path so the caller can delete the file if asked.
    pub fn delete_track(&self, video_id: &str) -> rusqlite::Result<Option<(Option<String>, Option<String>)>> {
        let conn = self.conn();
        let paths = conn
            .query_row("SELECT file_path, cover_path FROM tracks WHERE video_id = ?1", [video_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if paths.is_some() {
            conn.execute("DELETE FROM tracks WHERE video_id = ?1", [video_id])?;
        }
        Ok(paths)
    }

    pub fn artists(&self) -> rusqlite::Result<Vec<Artist>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT artist, COUNT(*), MIN(video_id) FROM tracks
             WHERE file_path IS NOT NULL
             GROUP BY artist COLLATE NOCASE ORDER BY artist COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| Ok(Artist { name: r.get(0)?, track_count: r.get(1)?, cover_track_id: r.get(2)? }))?;
        rows.collect()
    }

    pub fn albums(&self) -> rusqlite::Result<Vec<Album>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT album, artist, COUNT(*), MIN(video_id) FROM tracks
             WHERE file_path IS NOT NULL AND album != ''
             GROUP BY album COLLATE NOCASE, artist COLLATE NOCASE
             ORDER BY album COLLATE NOCASE",
        )?;
        let rows =
            stmt.query_map([], |r| Ok(Album { name: r.get(0)?, artist: r.get(1)?, track_count: r.get(2)?, cover_track_id: r.get(3)? }))?;
        rows.collect()
    }

    /// Seeds for the taste profile: most played plus most recently added.
    pub fn taste_seeds(&self, per_kind: usize) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut seeds: Vec<String> = Vec::new();
        for order in ["play_count DESC, last_played DESC", "added_at DESC, rowid DESC"] {
            let mut stmt = conn.prepare(&format!("SELECT video_id FROM tracks WHERE file_path IS NOT NULL ORDER BY {order} LIMIT ?1"))?;
            for id in stmt.query_map([per_kind as i64], |r| r.get::<_, String>(0))? {
                let id = id?;
                if !seeds.contains(&id) {
                    seeds.push(id);
                }
            }
        }
        Ok(seeds)
    }

    // ------------------------------------------------------------- playlists

    fn playlist_summary(conn: &Connection, id: i64) -> rusqlite::Result<Playlist> {
        let (name, kind, source_url, created_at, updated_at) =
            conn.query_row("SELECT name, kind, source_url, created_at, updated_at FROM playlists WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
        let (track_count, duration_sec) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(t.duration_sec), 0) FROM playlist_tracks pt
             JOIN tracks t ON t.video_id = pt.video_id WHERE pt.playlist_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut stmt = conn.prepare("SELECT video_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position LIMIT 4")?;
        let cover_track_ids = stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(Playlist { id, name, kind, source_url, track_count, duration_sec, cover_track_ids, created_at, updated_at })
    }

    pub fn playlists(&self) -> rusqlite::Result<Vec<Playlist>> {
        let conn = self.conn();
        let ids: Vec<i64> = {
            // Discover Mix first, then newest playlists.
            let mut stmt = conn.prepare("SELECT id FROM playlists ORDER BY kind = 'discover' DESC, updated_at DESC")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        ids.into_iter().map(|id| Self::playlist_summary(&conn, id)).collect()
    }

    pub fn playlist(&self, id: i64) -> rusqlite::Result<PlaylistDetail> {
        let conn = self.conn();
        let playlist = Self::playlist_summary(&conn, id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM playlist_tracks pt JOIN tracks t ON t.video_id = pt.video_id
             WHERE pt.playlist_id = ?1 ORDER BY pt.position",
            TRACK_COLS.split(", ").map(|c| format!("t.{c}")).collect::<Vec<_>>().join(", ")
        ))?;
        let tracks = stmt.query_map([id], track_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(PlaylistDetail { playlist, tracks })
    }

    pub fn create_playlist(&self, name: &str, kind: &str, source_url: Option<&str>) -> rusqlite::Result<i64> {
        let now = now_ms();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO playlists (name, kind, source_url, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![name, kind, source_url, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn find_playlist_by_kind(&self, kind: &str) -> rusqlite::Result<Option<i64>> {
        self.conn().query_row("SELECT id FROM playlists WHERE kind = ?1 ORDER BY id LIMIT 1", [kind], |r| r.get(0)).optional()
    }

    fn touch_playlist(conn: &Connection, id: i64) -> rusqlite::Result<usize> {
        conn.execute("UPDATE playlists SET updated_at = ?2 WHERE id = ?1", params![id, now_ms()])
    }

    /// Returns false if the playlist does not exist.
    pub fn rename_playlist(&self, id: i64, name: &str) -> rusqlite::Result<bool> {
        let n = self.conn().execute("UPDATE playlists SET name = ?2, updated_at = ?3 WHERE id = ?1", params![id, name, now_ms()])?;
        Ok(n > 0)
    }

    /// Deletes only the playlist; music files and library songs stay.
    pub fn delete_playlist(&self, id: i64) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("DELETE FROM playlists WHERE id = ?1", [id])? > 0)
    }

    /// Append a song to the end. Adding a song that is already there is a no-op.
    pub fn add_to_playlist(&self, id: i64, video_id: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO playlist_tracks (playlist_id, video_id, position)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_tracks WHERE playlist_id = ?1))",
            params![id, video_id],
        )?;
        Self::touch_playlist(&conn, id)?;
        Ok(())
    }

    pub fn remove_from_playlist(&self, id: i64, video_id: &str) -> rusqlite::Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let removed = tx.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND video_id = ?2", params![id, video_id])? > 0;
        if removed {
            Self::compact_positions(&tx, id)?;
            Self::touch_playlist(&tx, id)?;
        }
        tx.commit()?;
        Ok(removed)
    }

    fn compact_positions(conn: &Connection, id: i64) -> rusqlite::Result<()> {
        let ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT video_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position")?;
            let rows = stmt.query_map([id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut upd = conn.prepare("UPDATE playlist_tracks SET position = ?3 WHERE playlist_id = ?1 AND video_id = ?2")?;
        for (pos, vid) in ids.iter().enumerate() {
            upd.execute(params![id, vid, pos as i64])?;
        }
        Ok(())
    }

    /// Set a new order. `track_ids` must contain exactly the playlist's songs.
    pub fn reorder_playlist(&self, id: i64, track_ids: &[String]) -> Result<(), ReorderError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut current: Vec<String> = {
            let mut stmt = tx.prepare("SELECT video_id FROM playlist_tracks WHERE playlist_id = ?1")?;
            let rows = stmt.query_map([id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut wanted = track_ids.to_vec();
        current.sort();
        wanted.sort();
        if current != wanted {
            return Err(ReorderError::Mismatch);
        }
        {
            let mut upd = tx.prepare("UPDATE playlist_tracks SET position = ?3 WHERE playlist_id = ?1 AND video_id = ?2")?;
            for (pos, vid) in track_ids.iter().enumerate() {
                upd.execute(params![id, vid, pos as i64])?;
            }
        }
        Self::touch_playlist(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Replace all songs of a playlist (used for imports and the Discover Mix).
    pub fn set_playlist_tracks(&self, id: i64, video_ids: &[String]) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", [id])?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO playlist_tracks (playlist_id, video_id, position) VALUES (?1, ?2, ?3)")?;
            for (pos, vid) in video_ids.iter().enumerate() {
                ins.execute(params![id, vid, pos as i64])?;
            }
        }
        Self::compact_positions(&tx, id)?;
        Self::touch_playlist(&tx, id)?;
        tx.commit()
    }

    // ------------------------------------------------------------- downloads

    pub fn insert_download(&self, d: &Download) -> rusqlite::Result<()> {
        self.conn().execute(
            &format!("INSERT INTO downloads ({DOWNLOAD_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
            params![d.id, d.video_id, d.title, d.artist, d.status, d.progress, d.speed, d.error, d.playlist_id, d.created_at, d.updated_at],
        )?;
        Ok(())
    }

    pub fn update_download(&self, d: &Download) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET status = ?2, progress = ?3, speed = ?4, error = ?5, updated_at = ?6,
                title = ?7, artist = ?8 WHERE id = ?1",
            params![d.id, d.status, d.progress, d.speed, d.error, d.updated_at, d.title, d.artist],
        )?;
        Ok(())
    }

    pub fn get_download(&self, id: &str) -> rusqlite::Result<Option<Download>> {
        self.conn().query_row(&format!("SELECT {DOWNLOAD_COLS} FROM downloads WHERE id = ?1"), [id], download_from_row).optional()
    }

    pub fn active_download_for(&self, video_id: &str) -> rusqlite::Result<Option<Download>> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {DOWNLOAD_COLS} FROM downloads
                     WHERE video_id = ?1 AND status IN ('queued', 'downloading') LIMIT 1"
                ),
                [video_id],
                download_from_row,
            )
            .optional()
    }

    pub fn downloads(&self, limit: usize) -> rusqlite::Result<Vec<Download>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DOWNLOAD_COLS} FROM downloads
             ORDER BY CASE status WHEN 'downloading' THEN 0 WHEN 'queued' THEN 1 WHEN 'failed' THEN 2 ELSE 3 END,
                      created_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], download_from_row)?;
        rows.collect()
    }

    /// Downloads that were running when the app last quit can't resume.
    pub fn fail_interrupted_downloads(&self) -> rusqlite::Result<usize> {
        self.conn().execute(
            "UPDATE downloads SET status = 'failed', error = 'Interrupted (app was closed)', speed = NULL,
                updated_at = ?1 WHERE status IN ('queued', 'downloading')",
            [now_ms()],
        )
    }

    /// Remove one finished download record (never a running one, never the file).
    /// Returns false when there is no such record or it is still active.
    pub fn delete_download(&self, id: &str) -> rusqlite::Result<bool> {
        let n = self.conn().execute("DELETE FROM downloads WHERE id = ?1 AND status NOT IN ('queued', 'downloading')", [id])?;
        Ok(n > 0)
    }

    /// Bulk-remove finished download records. `status` limits it to "done"
    /// (also clears "skipped"), "failed", or None for all finished ones;
    /// `ids` limits it to those records. Active downloads are never removed.
    pub fn clear_downloads(&self, status: Option<&str>, ids: Option<&[String]>) -> rusqlite::Result<usize> {
        let status_sql = match status {
            Some("done") => "status IN ('done', 'skipped')",
            Some("failed") => "status = 'failed'",
            _ => "status NOT IN ('queued', 'downloading')",
        };
        let conn = self.conn();
        match ids {
            None => conn.execute(&format!("DELETE FROM downloads WHERE {status_sql}"), []),
            Some(ids) => {
                let mut stmt = conn.prepare(&format!("DELETE FROM downloads WHERE id = ?1 AND {status_sql}"))?;
                let mut n = 0;
                for id in ids {
                    n += stmt.execute([id])?;
                }
                Ok(n)
            }
        }
    }

    /// Drop older failed records for a song (a new attempt replaces them).
    pub fn delete_failed_for(&self, video_id: &str, keep_id: &str) -> rusqlite::Result<usize> {
        self.conn().execute("DELETE FROM downloads WHERE video_id = ?1 AND status = 'failed' AND id != ?2", params![video_id, keep_id])
    }

    /// The newest failed record for a song, if any.
    #[cfg(test)]
    pub fn failed_download_for(&self, video_id: &str) -> rusqlite::Result<Option<Download>> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {DOWNLOAD_COLS} FROM downloads WHERE video_id = ?1 AND status = 'failed'
                     ORDER BY updated_at DESC LIMIT 1"
                ),
                [video_id],
                download_from_row,
            )
            .optional()
    }

    /// Failed records whose song is in the library by now are stale: remove
    /// them, and collapse repeated failures of one song into the newest.
    pub fn prune_failed_downloads(&self) -> rusqlite::Result<usize> {
        let conn = self.conn();
        let resolved = conn.execute(
            "DELETE FROM downloads WHERE status = 'failed' AND video_id IN
                (SELECT video_id FROM tracks WHERE file_path IS NOT NULL)",
            [],
        )?;
        let duplicates = conn.execute(
            "DELETE FROM downloads WHERE status = 'failed' AND id NOT IN
                (SELECT id FROM (SELECT id, ROW_NUMBER() OVER
                    (PARTITION BY video_id ORDER BY updated_at DESC) AS rn
                 FROM downloads WHERE status = 'failed') WHERE rn = 1)",
            [],
        )?;
        Ok(resolved + duplicates)
    }

    // ---------------------------------------------------------------- lyrics

    /// Cached lyrics JSON and whether any were found, if fetched before.
    pub fn get_lyrics(&self, video_id: &str) -> rusqlite::Result<Option<(String, bool, i64)>> {
        self.conn()
            .query_row("SELECT json, found, fetched_at FROM lyrics WHERE video_id = ?1", [video_id], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?))
            })
            .optional()
    }

    pub fn set_lyrics(&self, video_id: &str, json: &str, found: bool) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO lyrics (video_id, json, found, fetched_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(video_id) DO UPDATE SET json = excluded.json, found = excluded.found,
                fetched_at = excluded.fetched_at",
            params![video_id, json, found as i64, now_ms()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------- recommendations

    pub fn replace_recommendations(&self, recs: &[Recommendation]) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM recommendations", [])?;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO recommendations (video_id, rank, score, because_video_id, because_artist)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (rank, r) in recs.iter().enumerate() {
                ins.execute(params![r.track.video_id, rank as i64, r.score, r.because_video_id, r.because_artist])?;
            }
        }
        tx.commit()
    }

    pub fn recommendations(&self, limit: usize) -> rusqlite::Result<Vec<Recommendation>> {
        let conn = self.conn();
        let cols = TRACK_COLS.split(", ").map(|c| format!("t.{c}")).collect::<Vec<_>>().join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {cols}, r.score, r.because_video_id, r.because_artist
             FROM recommendations r JOIN tracks t ON t.video_id = r.video_id
             WHERE t.file_path IS NULL
             ORDER BY r.rank LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(Recommendation { track: track_from_row(r)?, score: r.get(10)?, because_video_id: r.get(11)?, because_artist: r.get(12)? })
        })?;
        rows.collect()
    }

    // ------------------------------------------------------------------ meta

    pub fn get_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn().query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

/// A paired device (see `sync.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    /// `ip:port` of its sync listener.
    pub address: String,
    pub dns_name: Option<String>,
    /// Shared secret both sides send as `Authorization: Bearer`.
    pub token: String,
    pub last_sync_at: Option<i64>,
    pub last_error: Option<String>,
}

const PEER_COLS: &str = "id, name, address, dns_name, token, last_sync_at, last_error";

fn peer_from_row(r: &Row) -> rusqlite::Result<Peer> {
    Ok(Peer {
        id: r.get(0)?,
        name: r.get(1)?,
        address: r.get(2)?,
        dns_name: r.get(3)?,
        token: r.get(4)?,
        last_sync_at: r.get(5)?,
        last_error: r.get(6)?,
    })
}

impl Db {
    // ------------------------------------------------------------------ sync

    pub fn peers(&self) -> rusqlite::Result<Vec<Peer>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {PEER_COLS} FROM peers ORDER BY created_at"))?;
        let rows = stmt.query_map([], peer_from_row)?;
        rows.collect()
    }

    pub fn get_peer(&self, id: &str) -> rusqlite::Result<Option<Peer>> {
        self.conn().query_row(&format!("SELECT {PEER_COLS} FROM peers WHERE id = ?1"), [id], peer_from_row).optional()
    }

    /// Store a newly paired device; pairing the same device again replaces
    /// its name, address and token but keeps its sync history.
    pub fn upsert_peer(&self, p: &Peer) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO peers (id, name, address, dns_name, token, last_sync_at, last_error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, address = excluded.address,
                dns_name = COALESCE(excluded.dns_name, peers.dns_name), token = excluded.token,
                last_error = NULL",
            params![p.id, p.name, p.address, p.dns_name, p.token, now_ms()],
        )?;
        Ok(())
    }

    pub fn delete_peer(&self, id: &str) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("DELETE FROM peers WHERE id = ?1", [id])? > 0)
    }

    pub fn set_peer_address(&self, id: &str, address: &str, dns_name: Option<&str>) -> rusqlite::Result<()> {
        self.conn()
            .execute("UPDATE peers SET address = ?2, dns_name = COALESCE(?3, dns_name) WHERE id = ?1", params![id, address, dns_name])?;
        Ok(())
    }

    /// Record the outcome of a sync: success sets `last_sync_at` and clears
    /// the error, failure only sets the error.
    pub fn set_peer_sync_result(&self, id: &str, error: Option<&str>) -> rusqlite::Result<()> {
        let conn = self.conn();
        match error {
            None => conn.execute("UPDATE peers SET last_sync_at = ?2, last_error = NULL WHERE id = ?1", params![id, now_ms()])?,
            Some(e) => conn.execute("UPDATE peers SET last_error = ?2 WHERE id = ?1", params![id, e])?,
        };
        Ok(())
    }

    /// Store a song received from a paired device: metadata from the peer,
    /// the local file path; keeps local play history and cover.
    pub fn upsert_synced_track(&self, t: &Track, file_path: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO tracks (video_id, title, artist, album, duration_sec, thumbnail_url, file_path, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(video_id) DO UPDATE SET
                title = excluded.title, artist = excluded.artist, album = excluded.album,
                duration_sec = excluded.duration_sec, thumbnail_url = excluded.thumbnail_url,
                file_path = excluded.file_path,
                added_at = COALESCE(tracks.added_at, excluded.added_at)",
            params![t.video_id, t.title, t.artist, t.album, t.duration_sec, t.thumbnail_url, file_path, now_ms()],
        )?;
        Ok(())
    }

    /// Every playlist except the generated Discover Mix, with its songs in order.
    pub fn playlists_for_sync(&self) -> rusqlite::Result<Vec<(String, String, Vec<String>)>> {
        let conn = self.conn();
        let lists: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare("SELECT id, name, kind FROM playlists WHERE kind != 'discover' ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut stmt = conn.prepare("SELECT video_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position")?;
        let mut out = Vec::new();
        for (id, name, kind) in lists {
            let ids = stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            out.push((name, kind, ids));
        }
        Ok(out)
    }

    /// Additive playlist merge: find the playlist by name (never the Discover
    /// Mix), create it if missing, then append the given songs it doesn't
    /// have yet in the given order. Songs unknown locally are skipped; nothing
    /// is ever removed. Returns how many songs were added (and whether the
    /// playlist was created).
    pub fn merge_playlist(&self, name: &str, kind: &str, video_ids: &[String]) -> rusqlite::Result<(usize, bool)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM playlists WHERE name = ?1 AND kind != 'discover' ORDER BY id LIMIT 1", [name], |r| r.get(0))
            .optional()?;
        let known: Vec<&String> = {
            let mut stmt = tx.prepare("SELECT 1 FROM tracks WHERE video_id = ?1")?;
            let mut known = Vec::new();
            for id in video_ids {
                if stmt.exists([id])? {
                    known.push(id);
                }
            }
            known
        };
        let (id, created) = match existing {
            Some(id) => (id, false),
            None => {
                let kind = if kind == "imported" { "imported" } else { "user" };
                let now = now_ms();
                tx.execute(
                    "INSERT INTO playlists (name, kind, source_url, created_at, updated_at) VALUES (?1, ?2, NULL, ?3, ?3)",
                    params![name, kind, now],
                )?;
                (tx.last_insert_rowid(), true)
            }
        };
        let mut added = 0;
        {
            let mut ins = tx.prepare(
                "INSERT OR IGNORE INTO playlist_tracks (playlist_id, video_id, position)
                 VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_tracks WHERE playlist_id = ?1))",
            )?;
            for vid in known {
                added += ins.execute(params![id, vid])?;
            }
        }
        if added > 0 {
            Self::touch_playlist(&tx, id)?;
        }
        tx.commit()?;
        Ok((added, created))
    }

    #[cfg(test)]
    pub fn count_plays(&self, video_id: &str) -> rusqlite::Result<i64> {
        self.conn().query_row("SELECT COUNT(*) FROM plays WHERE video_id = ?1", [video_id], |r| r.get(0))
    }

    pub fn private_listening(&self) -> rusqlite::Result<bool> {
        Ok(self.get_meta(PRIVATE_LISTENING_KEY)?.as_deref() == Some("1"))
    }

    pub fn set_private_listening(&self, enabled: bool) -> rusqlite::Result<()> {
        self.set_meta(PRIVATE_LISTENING_KEY, if enabled { "1" } else { "0" })
    }
}

// ------------------------------------------------------------------ podcasts

const PODCAST_COLS: &str =
    "p.id, p.title, p.author, p.feed_url, p.image_url, p.subscribed, p.subscribed_at, p.last_checked, p.last_error, \
     (SELECT COUNT(*) FROM podcast_episodes e WHERE e.podcast_id = p.id), \
     (SELECT COUNT(*) FROM podcast_episodes e WHERE e.podcast_id = p.id AND e.played = 0)";

fn podcast_from_row(r: &Row) -> rusqlite::Result<Podcast> {
    Ok(Podcast {
        id: r.get(0)?,
        title: r.get(1)?,
        author: r.get(2)?,
        feed_url: r.get(3)?,
        image_url: r.get(4)?,
        subscribed: r.get::<_, i64>(5)? != 0,
        subscribed_at: r.get(6)?,
        last_checked: r.get(7)?,
        last_error: r.get(8)?,
        episode_count: r.get(9)?,
        unplayed_count: r.get(10)?,
    })
}

const EPISODE_COLS: &str =
    "id, podcast_id, title, published_at, duration_sec, enclosure_url, file_path, status, progress, error, position_sec, played";

fn episode_from_row(r: &Row) -> rusqlite::Result<Episode> {
    Ok(Episode {
        id: r.get(0)?,
        podcast_id: r.get(1)?,
        title: r.get(2)?,
        published_at: r.get(3)?,
        duration_sec: r.get(4)?,
        enclosure_url: r.get(5)?,
        file_path: r.get(6)?,
        status: r.get(7)?,
        progress: r.get(8)?,
        error: r.get(9)?,
        position_sec: r.get(10)?,
        played: r.get::<_, i64>(11)? != 0,
    })
}

impl Db {
    pub fn podcast(&self, id: i64) -> rusqlite::Result<Option<Podcast>> {
        let sql = format!("SELECT {PODCAST_COLS} FROM podcasts p WHERE p.id = ?1");
        self.conn().query_row(&sql, [id], podcast_from_row).optional()
    }

    pub fn podcast_by_feed(&self, feed_url: &str) -> rusqlite::Result<Option<Podcast>> {
        let sql = format!("SELECT {PODCAST_COLS} FROM podcasts p WHERE p.feed_url = ?1");
        self.conn().query_row(&sql, [feed_url], podcast_from_row).optional()
    }

    /// Subscribed podcasts, A to Z.
    pub fn subscribed_podcasts(&self) -> rusqlite::Result<Vec<Podcast>> {
        let sql = format!("SELECT {PODCAST_COLS} FROM podcasts p WHERE p.subscribed = 1 ORDER BY p.title COLLATE NOCASE ASC, p.id");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], podcast_from_row)?;
        rows.collect()
    }

    pub fn insert_podcast(&self, feed_url: &str, title: &str, author: &str, image_url: &str, now: i64) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO podcasts (feed_url, title, author, image_url, subscribed, subscribed_at) VALUES (?1, ?2, ?3, ?4, 1, ?5)",
            params![feed_url, title, author, image_url, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Subscribe again: episodes that came out meanwhile are not auto-downloaded.
    pub fn resubscribe_podcast(&self, id: i64, now: i64, title: &str, author: &str, image_url: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE podcasts SET subscribed = 1, subscribed_at = CASE WHEN subscribed = 1 THEN subscribed_at ELSE ?2 END, \
             title = ?3, author = ?4, image_url = CASE WHEN ?5 = '' THEN image_url ELSE ?5 END WHERE id = ?1",
            params![id, now, title, author, image_url],
        )?;
        Ok(())
    }

    pub fn update_podcast_meta(&self, id: i64, title: &str, author: &str, image_url: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("UPDATE podcasts SET title = ?2, author = ?3, image_url = ?4 WHERE id = ?1", params![id, title, author, image_url])?;
        Ok(())
    }

    pub fn set_podcast_checked(&self, id: i64, at: i64, error: Option<&str>) -> rusqlite::Result<()> {
        self.conn().execute("UPDATE podcasts SET last_checked = ?2, last_error = ?3 WHERE id = ?1", params![id, at, error])?;
        Ok(())
    }

    /// Stop following; episodes, positions and downloaded files stay.
    pub fn unsubscribe_podcast(&self, id: i64) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("UPDATE podcasts SET subscribed = 0 WHERE id = ?1", [id])? > 0)
    }

    /// Add new episodes and refresh the title / link / length of known ones.
    /// Returns how many are new.
    pub fn upsert_episodes(&self, podcast_id: i64, eps: &[FeedEpisode]) -> rusqlite::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut new = 0;
        {
            let mut exists = tx.prepare("SELECT 1 FROM podcast_episodes WHERE podcast_id = ?1 AND guid = ?2")?;
            let mut insert = tx.prepare(
                "INSERT INTO podcast_episodes (podcast_id, guid, title, published_at, duration_sec, enclosure_url) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut update = tx.prepare(
                "UPDATE podcast_episodes SET title = ?3, enclosure_url = ?4, \
                 published_at = CASE WHEN ?5 > 0 THEN ?5 ELSE published_at END, \
                 duration_sec = CASE WHEN ?6 > 0 THEN ?6 ELSE duration_sec END WHERE podcast_id = ?1 AND guid = ?2",
            )?;
            for e in eps {
                if exists.exists(params![podcast_id, e.guid])? {
                    update.execute(params![podcast_id, e.guid, e.title, e.enclosure_url, e.published_at, e.duration_sec])?;
                } else {
                    insert.execute(params![podcast_id, e.guid, e.title, e.published_at, e.duration_sec, e.enclosure_url])?;
                    new += 1;
                }
            }
        }
        tx.commit()?;
        Ok(new)
    }

    pub fn episode(&self, id: i64) -> rusqlite::Result<Option<Episode>> {
        let sql = format!("SELECT {EPISODE_COLS} FROM podcast_episodes WHERE id = ?1");
        self.conn().query_row(&sql, [id], episode_from_row).optional()
    }

    /// Newest first.
    pub fn episodes(&self, podcast_id: i64, limit: usize) -> rusqlite::Result<Vec<Episode>> {
        let sql = format!("SELECT {EPISODE_COLS} FROM podcast_episodes WHERE podcast_id = ?1 ORDER BY published_at DESC, id DESC LIMIT ?2");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![podcast_id, limit as i64], episode_from_row)?;
        rows.collect()
    }

    /// Episodes published after subscribing that are not downloaded, queued
    /// or deleted by hand.
    pub fn auto_download_candidates(&self, podcast_id: i64) -> rusqlite::Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT e.id FROM podcast_episodes e JOIN podcasts p ON p.id = e.podcast_id \
             WHERE e.podcast_id = ?1 AND p.subscribed = 1 AND e.published_at > p.subscribed_at \
               AND e.status IN ('none', 'failed') AND e.no_auto = 0 ORDER BY e.published_at",
        )?;
        let rows = stmt.query_map([podcast_id], |r| r.get(0))?;
        rows.collect()
    }

    /// Move an episode from "not downloaded" / "failed" to "queued". False if
    /// it is already queued, downloading or done.
    pub fn claim_episode(&self, id: i64) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "UPDATE podcast_episodes SET status = 'queued', progress = 0, error = NULL WHERE id = ?1 \
             AND status IN ('none', 'failed')",
            [id],
        )?;
        Ok(n > 0)
    }

    pub fn set_episode_state(&self, id: i64, status: &str, progress: f64, error: Option<&str>) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE podcast_episodes SET status = ?2, progress = ?3, error = ?4 WHERE id = ?1",
            params![id, status, progress, error],
        )?;
        Ok(())
    }

    pub fn set_episode_done(&self, id: i64, file_path: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE podcast_episodes SET status = 'done', progress = 100, error = NULL, file_path = ?2 WHERE id = ?1",
            params![id, file_path],
        )?;
        Ok(())
    }

    /// The file is gone: back to "not downloaded", and never auto-download it again.
    pub fn clear_episode_file(&self, id: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE podcast_episodes SET status = 'none', progress = 0, error = NULL, file_path = NULL, no_auto = 1 WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Set the position; `duration` (when known) fills in a missing length.
    pub fn set_episode_position(&self, id: i64, position: f64, played: bool, duration: Option<i64>) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE podcast_episodes SET position_sec = ?2, played = CASE WHEN ?3 THEN 1 ELSE played END, \
             duration_sec = CASE WHEN duration_sec = 0 AND ?4 > 0 THEN ?4 ELSE duration_sec END WHERE id = ?1",
            params![id, position, played, duration.unwrap_or(0)],
        )?;
        Ok(())
    }

    /// Downloads cut off by a crash or shutdown go back to "not downloaded".
    pub fn reset_interrupted_episodes(&self) -> rusqlite::Result<usize> {
        self.conn().execute("UPDATE podcast_episodes SET status = 'none', progress = 0 WHERE status IN ('queued', 'downloading')", [])
    }
}

pub const PRIVATE_LISTENING_KEY: &str = "private_listening";

#[derive(Debug)]
pub enum ReorderError {
    Mismatch,
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for ReorderError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, title: &str, artist: &str) -> Track {
        Track {
            video_id: id.into(),
            title: title.into(),
            artist: artist.into(),
            album: "Album".into(),
            duration_sec: 200,
            ..Default::default()
        }
    }

    fn db_with_library(n: usize) -> Db {
        let db = Db::open_in_memory().unwrap();
        for i in 0..n {
            let t = track(&format!("v{i}"), &format!("Song {i}"), "Artist");
            db.mark_downloaded(&t, &format!("/music/{i}.mp3"), None).unwrap();
        }
        db
    }

    fn ids(db: &Db, playlist: i64) -> Vec<String> {
        db.playlist(playlist).unwrap().tracks.into_iter().map(|t| t.video_id).collect()
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = std::env::temp_dir().join(format!("geonix-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.db");
        Db::open(&path).unwrap();
        let db = Db::open(&path).unwrap();
        assert!(db.playlists().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn library_only_contains_downloaded_tracks() {
        let db = db_with_library(2);
        db.upsert_track_meta(&track("remote", "Search Result", "Someone")).unwrap();
        let lib = db.library_tracks(None, None).unwrap();
        assert_eq!(lib.len(), 2);
        assert!(lib.iter().all(|t| t.downloaded));
        assert!(!db.get_track("remote").unwrap().unwrap().downloaded);
    }

    #[test]
    fn meta_upsert_does_not_clobber_downloaded_track() {
        let db = db_with_library(1);
        db.upsert_track_meta(&track("v0", "Changed", "Other")).unwrap();
        let t = db.get_track("v0").unwrap().unwrap();
        assert_eq!(t.title, "Song 0");
        assert_eq!(t.file_path.as_deref(), Some("/music/0.mp3"));
    }

    #[test]
    fn playlist_add_appends_in_order_and_ignores_duplicates() {
        let db = db_with_library(3);
        let p = db.create_playlist("Mix", "user", None).unwrap();
        for id in ["v2", "v0", "v1", "v0"] {
            db.add_to_playlist(p, id).unwrap();
        }
        assert_eq!(ids(&db, p), ["v2", "v0", "v1"]);
        let summary = db.playlist(p).unwrap().playlist;
        assert_eq!(summary.track_count, 3);
        assert_eq!(summary.duration_sec, 600);
        assert_eq!(summary.cover_track_ids, ["v2", "v0", "v1"]);
    }

    #[test]
    fn playlist_reorder_and_remove_keep_positions_dense() {
        let db = db_with_library(4);
        let p = db.create_playlist("Mix", "user", None).unwrap();
        db.set_playlist_tracks(p, &["v0", "v1", "v2", "v3"].map(String::from)).unwrap();

        db.reorder_playlist(p, &["v3", "v1", "v0", "v2"].map(String::from)).unwrap();
        assert_eq!(ids(&db, p), ["v3", "v1", "v0", "v2"]);

        assert!(db.remove_from_playlist(p, "v1").unwrap());
        assert_eq!(ids(&db, p), ["v3", "v0", "v2"]);
        db.add_to_playlist(p, "v1").unwrap();
        assert_eq!(ids(&db, p), ["v3", "v0", "v2", "v1"]);

        let positions: Vec<i64> = {
            let conn = db.conn();
            let mut stmt = conn.prepare("SELECT position FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position").unwrap();
            let rows = stmt.query_map([p], |r| r.get(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(positions, [0, 1, 2, 3]);
    }

    #[test]
    fn reorder_rejects_wrong_track_set() {
        let db = db_with_library(3);
        let p = db.create_playlist("Mix", "user", None).unwrap();
        db.set_playlist_tracks(p, &["v0", "v1", "v2"].map(String::from)).unwrap();
        let err = db.reorder_playlist(p, &["v0", "v1"].map(String::from));
        assert!(matches!(err, Err(ReorderError::Mismatch)));
        let err = db.reorder_playlist(p, &["v0", "v1", "nope"].map(String::from));
        assert!(matches!(err, Err(ReorderError::Mismatch)));
        assert_eq!(ids(&db, p), ["v0", "v1", "v2"]);
    }

    #[test]
    fn deleting_playlist_keeps_library_tracks() {
        let db = db_with_library(2);
        let p = db.create_playlist("Mix", "user", None).unwrap();
        db.add_to_playlist(p, "v0").unwrap();
        assert!(db.delete_playlist(p).unwrap());
        assert_eq!(db.library_tracks(None, None).unwrap().len(), 2);
        assert!(db.playlists().unwrap().is_empty());
    }

    #[test]
    fn deleting_track_removes_it_from_playlists() {
        let db = db_with_library(2);
        let p = db.create_playlist("Mix", "user", None).unwrap();
        db.set_playlist_tracks(p, &["v0", "v1"].map(String::from)).unwrap();
        let paths = db.delete_track("v0").unwrap().unwrap();
        assert_eq!(paths.0.as_deref(), Some("/music/0.mp3"));
        assert_eq!(ids(&db, p), ["v1"]);
    }

    #[test]
    fn plays_update_count_sort_and_seeds() {
        let db = db_with_library(3);
        for _ in 0..3 {
            db.record_play("v1").unwrap();
        }
        db.record_play("v2").unwrap();
        assert!(db.record_play("missing").unwrap().is_none());
        let by_plays = db.library_tracks(Some("plays"), None).unwrap();
        assert_eq!(by_plays[0].video_id, "v1");
        assert_eq!(by_plays[0].play_count, 3);
        let seeds = db.taste_seeds(1).unwrap();
        assert_eq!(seeds[0], "v1");
        assert_eq!(seeds.len(), 2);
    }

    #[test]
    fn text_filter_matches_title_artist_album() {
        let db = Db::open_in_memory().unwrap();
        db.mark_downloaded(&track("a", "One More Time", "Daft Punk"), "/a.mp3", None).unwrap();
        db.mark_downloaded(&track("b", "Breathe", "Telepopmusik"), "/b.mp3", None).unwrap();
        assert_eq!(db.library_tracks(None, Some("daft")).unwrap().len(), 1);
        assert_eq!(db.library_tracks(None, Some("ALBUM")).unwrap().len(), 2);
        assert_eq!(db.library_tracks(None, Some("zzz")).unwrap().len(), 0);
    }

    fn download(id: &str, video_id: &str, status: &str, at: i64) -> Download {
        Download {
            id: id.into(),
            video_id: video_id.into(),
            title: String::new(),
            artist: String::new(),
            status: status.into(),
            progress: 0.0,
            speed: None,
            error: None,
            playlist_id: None,
            created_at: at,
            updated_at: at,
        }
    }

    fn download_ids(db: &Db) -> Vec<String> {
        let mut ids: Vec<String> = db.downloads(100).unwrap().into_iter().map(|d| d.id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn clearing_downloads_never_touches_active_ones() {
        let db = Db::open_in_memory().unwrap();
        for (id, status) in [("a", "done"), ("b", "skipped"), ("c", "failed"), ("d", "queued"), ("e", "downloading")] {
            db.insert_download(&download(id, id, status, 1)).unwrap();
        }
        assert!(!db.delete_download("d").unwrap(), "queued downloads can't be deleted");
        assert!(db.delete_download("c").unwrap());
        assert_eq!(db.clear_downloads(Some("done"), None).unwrap(), 2);
        assert_eq!(download_ids(&db), ["d", "e"]);
        db.insert_download(&download("f", "f", "failed", 1)).unwrap();
        db.insert_download(&download("g", "g", "done", 1)).unwrap();
        let picked = ["f".to_string(), "d".to_string()];
        assert_eq!(db.clear_downloads(None, Some(&picked)).unwrap(), 1, "only the finished one in the selection");
        assert_eq!(download_ids(&db), ["d", "e", "g"]);
    }

    #[test]
    fn stale_and_duplicate_failures_are_pruned() {
        let db = db_with_library(1); // v0 is downloaded
        db.insert_download(&download("old0", "v0", "failed", 1)).unwrap();
        db.insert_download(&download("x1", "x", "failed", 1)).unwrap();
        db.insert_download(&download("x2", "x", "failed", 5)).unwrap();
        db.insert_download(&download("ok", "y", "done", 1)).unwrap();
        assert_eq!(db.prune_failed_downloads().unwrap(), 2);
        assert_eq!(download_ids(&db), ["ok", "x2"]);
        assert_eq!(db.failed_download_for("x").unwrap().unwrap().id, "x2");
        db.insert_download(&download("x3", "x", "queued", 9)).unwrap();
        assert_eq!(db.delete_failed_for("x", "x3").unwrap(), 1);
        assert!(db.failed_download_for("x").unwrap().is_none());
    }

    #[test]
    fn lyrics_cache_round_trip() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get_lyrics("v").unwrap().is_none());
        db.set_lyrics("v", r#"{"lines":[]}"#, false).unwrap();
        db.set_lyrics("v", r#"{"lines":[1]}"#, true).unwrap();
        let (json, found, _) = db.get_lyrics("v").unwrap().unwrap();
        assert_eq!(json, r#"{"lines":[1]}"#);
        assert!(found);
    }

    fn peer(id: &str) -> Peer {
        Peer {
            id: id.into(),
            name: format!("dev-{id}"),
            address: "100.64.0.2:7803".into(),
            dns_name: Some("dev.tail.ts.net".into()),
            token: "t1".into(),
            last_sync_at: None,
            last_error: None,
        }
    }

    #[test]
    fn peers_round_trip_and_repairing_updates() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_peer(&peer("a")).unwrap();
        db.set_peer_sync_result("a", None).unwrap();
        let mut again = peer("a");
        again.token = "t2".into();
        again.address = "100.64.0.9:7803".into();
        again.dns_name = None;
        db.upsert_peer(&again).unwrap();
        let p = db.get_peer("a").unwrap().unwrap();
        assert_eq!(p.token, "t2");
        assert_eq!(p.address, "100.64.0.9:7803");
        assert_eq!(p.dns_name.as_deref(), Some("dev.tail.ts.net"), "kept when not rediscovered");
        assert!(p.last_sync_at.is_some(), "re-pairing keeps the history");
        db.set_peer_sync_result("a", Some("offline")).unwrap();
        assert_eq!(db.get_peer("a").unwrap().unwrap().last_error.as_deref(), Some("offline"));
        assert_eq!(db.peers().unwrap().len(), 1);
        assert!(db.delete_peer("a").unwrap());
        assert!(db.get_peer("a").unwrap().is_none());
    }

    #[test]
    fn playlist_merge_is_additive_and_ordered_and_skips_discover() {
        let db = db_with_library(5);
        let discover = db.create_playlist("Discover Mix", "discover", None).unwrap();
        db.set_playlist_tracks(discover, &["v0".to_string()]).unwrap();
        let mine = db.create_playlist("Road", "user", None).unwrap();
        db.set_playlist_tracks(mine, &["v3", "v1"].map(String::from)).unwrap();

        // Peer order v0, v1, v4, unknown, v2: v1 is already there, unknown is skipped.
        let peer_ids = ["v0", "v1", "v4", "unknown", "v2"].map(String::from);
        assert_eq!(db.merge_playlist("Road", "user", &peer_ids).unwrap(), (3, false));
        assert_eq!(ids(&db, mine), ["v3", "v1", "v0", "v4", "v2"]);
        assert_eq!(db.merge_playlist("Road", "user", &peer_ids).unwrap(), (0, false), "second merge is a no-op");

        // A peer playlist named like the Discover Mix becomes a normal playlist.
        let (added, created) = db.merge_playlist("Discover Mix", "user", &["v2".to_string()]).unwrap();
        assert_eq!((added, created), (1, true));
        assert_eq!(ids(&db, discover), ["v0"], "the Discover Mix is never touched");

        let synced = db.playlists_for_sync().unwrap();
        assert!(synced.iter().all(|(_, kind, _)| kind != "discover"));
        assert_eq!(synced[0], ("Road".to_string(), "user".to_string(), ids(&db, mine)));
    }

    #[test]
    fn synced_track_keeps_play_history() {
        let db = db_with_library(1);
        db.record_play("v0").unwrap();
        let mut t = track("v0", "New Title", "Artist");
        t.play_count = 99;
        db.upsert_synced_track(&t, "/music/new.mp3").unwrap();
        let got = db.get_track("v0").unwrap().unwrap();
        assert_eq!(got.play_count, 1);
        assert_eq!(got.file_path.as_deref(), Some("/music/new.mp3"));
        db.upsert_synced_track(&track("fresh", "F", "A"), "/music/f.mp3").unwrap();
        assert!(db.get_track("fresh").unwrap().unwrap().downloaded);
    }

    #[test]
    fn private_listening_setting_round_trip() {
        let db = Db::open_in_memory().unwrap();
        assert!(!db.private_listening().unwrap());
        db.set_private_listening(true).unwrap();
        assert!(db.private_listening().unwrap());
        db.set_private_listening(false).unwrap();
        assert!(!db.private_listening().unwrap());
    }

    #[test]
    fn recommendations_hide_songs_that_got_downloaded() {
        let db = Db::open_in_memory().unwrap();
        let a = track("a", "A", "X");
        let b = track("b", "B", "Y");
        db.upsert_track_meta(&a).unwrap();
        db.upsert_track_meta(&b).unwrap();
        let recs: Vec<Recommendation> = [a.clone(), b]
            .into_iter()
            .map(|t| Recommendation { track: t, score: 1.0, because_video_id: None, because_artist: None })
            .collect();
        db.replace_recommendations(&recs).unwrap();
        assert_eq!(db.recommendations(10).unwrap().len(), 2);
        db.mark_downloaded(&a, "/a.mp3", None).unwrap();
        let left = db.recommendations(10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].track.video_id, "b");
    }
}
