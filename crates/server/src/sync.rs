//! Device sync: every device runs its own backend; paired backends copy each
//! other's songs and playlists over Tailscale.
//!
//! * Identity (`device_id`, `device_name`, `pair_code`) lives in the `meta`
//!   table; paired devices in the `peers` table.
//! * Pairing is by code only: the local backend asks every online Tailscale
//!   peer whether the code is theirs (`POST /sync/pair`); the accepting side
//!   generates a shared token and rotates its code.
//! * A sync with device X = pull everything new from X (additive, never
//!   deletes), then ask X to pull from us (`POST /sync/request`).
//! * The peer API runs on a second listener that only accepts Tailscale and
//!   loopback source addresses (see `is_allowed_peer_ip`).

use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::db::{Db, Peer};
use crate::downloads::DownloadManager;
use crate::error::{ApiError, ApiResult};
use crate::models::Track;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_SYNC_PORT: u16 = 7903;
/// A-Z without I and O (easy to read aloud and to type).
pub const CODE_ALPHABET: &[u8; 24] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
pub const CODE_LEN: usize = 6;
pub const DEVICE_HEADER: &str = "x-geonix-device";

const KEY_DEVICE_ID: &str = "device_id";
const KEY_DEVICE_NAME: &str = "device_name";
const KEY_PAIR_CODE: &str = "pair_code";

const PAIR_TIMEOUT: Duration = Duration::from_secs(2);
const HELLO_TIMEOUT: Duration = Duration::from_secs(1);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PAIR_FAILURES_PER_MINUTE: u32 = 10;

pub const NO_DEVICE_WITH_CODE: &str = "No device with that code found on your Tailscale network";

// ------------------------------------------------------------ randomness

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).expect("the OS random number generator is unavailable");
    buf
}

/// A fresh 6-letter pairing code from the OS RNG (rejection sampling keeps
/// every letter equally likely).
pub fn generate_code() -> String {
    let mut out = String::with_capacity(CODE_LEN);
    while out.len() < CODE_LEN {
        for b in random_bytes::<16>() {
            // 240 = 10 * 24: bytes above it would bias the first letters.
            if b < 240 && out.len() < CODE_LEN {
                out.push(CODE_ALPHABET[(b % 24) as usize] as char);
            }
        }
    }
    out
}

/// 32 random bytes as 64 hex characters.
pub fn generate_token() -> String {
    random_bytes::<32>().iter().map(|b| format!("{b:02x}")).collect()
}

/// Uppercase a typed code and check it uses only the code alphabet.
pub fn normalize_code(code: &str) -> Option<String> {
    let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// Compare secrets without leaking where they differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ------------------------------------------------------------ addresses

/// The peer listener only talks to Tailscale (100.64.0.0/10,
/// fd7a:115c:a1e0::/48) and this machine.
pub fn is_allowed_peer_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_allowed_peer_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback() || (s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0)
        }
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// `ip`, `ip:port`, `host` or `host:port` → something we can put in a URL.
pub fn normalize_address(address: &str, default_port: u16) -> String {
    let a = address.trim().trim_start_matches("http://").trim_end_matches('/');
    if let Ok(sa) = a.parse::<SocketAddr>() {
        return sa.to_string();
    }
    if let Ok(ip) = a.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
        return SocketAddr::new(ip, default_port).to_string();
    }
    if a.contains(':') {
        a.to_string()
    } else {
        format!("{a}:{default_port}")
    }
}

fn port_of(address: &str) -> Option<u16> {
    address.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

// ------------------------------------------------------------ file paths

/// A peer's `rel_path` made safe to join onto our music dir: no absolute
/// paths, drive letters or `..`; characters Windows can't store become `_`.
pub fn sanitize_rel_path(rel: &str) -> Option<PathBuf> {
    let rel = rel.trim();
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') || rel.chars().nth(1) == Some(':') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in rel.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return None,
            _ => {}
        }
        let clean: String = part.chars().map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c }).collect();
        let clean = clean.trim_end_matches(['.', ' ']).to_string();
        if clean.is_empty() {
            return None;
        }
        out.push(clean);
    }
    let safe = out.components().all(|c| matches!(c, Component::Normal(_)));
    (safe && out.components().next().is_some()).then_some(out)
}

/// Where a library file lives relative to the music dir, with `/`
/// separators. Files outside it are sent by file name only.
pub fn rel_path_for(file: &Path, music_dir: &Path) -> String {
    let rel = file
        .strip_prefix(music_dir)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let (f, m) = (file.canonicalize().ok()?, music_dir.canonicalize().ok()?);
            f.strip_prefix(m).ok().map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from(file.file_name().unwrap_or_default()));
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The file path if it exists and really lies inside the music dir.
pub fn file_inside(file: &Path, music_dir: &Path) -> Option<PathBuf> {
    let f = file.canonicalize().ok()?;
    let m = music_dir.canonicalize().ok()?;
    (f.starts_with(&m) && f.is_file()).then_some(f)
}

// ------------------------------------------------------------ misc

/// Milliseconds since the epoch as `2026-10-19T12:34:56Z`.
pub fn iso_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// This computer's name, shown on the other devices.
pub fn hostname() -> String {
    let from_env = ["GEONIX_DEVICE_NAME", "HOSTNAME", "COMPUTERNAME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty());
    let name = from_env
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let out = std::process::Command::new("hostname").output().ok()?;
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        })
        .unwrap_or_else(|| "Music Player".to_string());
    name.split('.').next().unwrap_or(&name).chars().take(64).collect()
}

/// At most `MAX_PAIR_FAILURES_PER_MINUTE` wrong codes per minute (all
/// callers together); after that every attempt gets 429 until the minute
/// that started with the first failure is over.
#[derive(Debug, Default)]
pub struct PairLimiter {
    window: Option<Instant>,
    failures: u32,
}

impl PairLimiter {
    const WINDOW: Duration = Duration::from_secs(60);

    pub fn blocked(&mut self, now: Instant) -> bool {
        match self.window {
            Some(start) if now.duration_since(start) < Self::WINDOW => self.failures >= MAX_PAIR_FAILURES_PER_MINUTE,
            _ => {
                self.window = None;
                self.failures = 0;
                false
            }
        }
    }

    pub fn fail(&mut self, now: Instant) {
        if self.blocked(now) {
            return;
        }
        self.window.get_or_insert(now);
        self.failures += 1;
    }
}

// ------------------------------------------------------------ identity

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThisDevice {
    pub id: String,
    pub name: String,
    pub code: String,
}

/// Create the device id and pairing code on first start; refresh the name.
pub fn ensure_identity(db: &Db) -> rusqlite::Result<()> {
    if db.get_meta(KEY_DEVICE_ID)?.filter(|s| !s.is_empty()).is_none() {
        db.set_meta(KEY_DEVICE_ID, &uuid::Uuid::new_v4().to_string())?;
    }
    db.set_meta(KEY_DEVICE_NAME, &hostname())?;
    if db.get_meta(KEY_PAIR_CODE)?.as_deref().and_then(normalize_code).is_none() {
        db.set_meta(KEY_PAIR_CODE, &generate_code())?;
    }
    Ok(())
}

pub fn this_device(db: &Db) -> rusqlite::Result<ThisDevice> {
    Ok(ThisDevice {
        id: db.get_meta(KEY_DEVICE_ID)?.unwrap_or_default(),
        name: db.get_meta(KEY_DEVICE_NAME)?.unwrap_or_default(),
        code: db.get_meta(KEY_PAIR_CODE)?.unwrap_or_default(),
    })
}

#[cfg(test)]
pub fn set_device_name(db: &Db, name: &str) {
    db.set_meta(KEY_DEVICE_NAME, name).unwrap();
}

pub fn rotate_code(db: &Db) -> rusqlite::Result<ThisDevice> {
    let old = db.get_meta(KEY_PAIR_CODE)?;
    let mut code = generate_code();
    while Some(&code) == old.as_ref() {
        code = generate_code();
    }
    db.set_meta(KEY_PAIR_CODE, &code)?;
    this_device(db)
}

// ------------------------------------------------------------ tailscale

#[derive(Debug, Clone, PartialEq)]
pub struct TsPeer {
    pub ip: IpAddr,
    pub dns_name: Option<String>,
    pub host_name: String,
    pub online: bool,
}

/// Where the Tailscale CLI is: $GEONIX_TAILSCALE, `tailscale` on PATH, then
/// the macOS app bundle and the Windows install folder.
pub fn tailscale_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GEONIX_TAILSCALE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let names: &[&str] = if cfg!(windows) { &["tailscale.exe", "tailscale"] } else { &["tailscale"] };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", r"C:\Program Files\Tailscale\tailscale.exe"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Peers from `tailscale status --json`.
pub fn parse_status(status: &serde_json::Value) -> Vec<TsPeer> {
    let Some(peers) = status.get("Peer").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    peers
        .values()
        .filter_map(|p| {
            let ips = p.get("TailscaleIPs")?.as_array()?;
            let ip = ips.first()?.as_str()?.parse::<IpAddr>().ok()?;
            let dns_name = p.get("DNSName").and_then(|d| d.as_str()).map(|d| d.trim_end_matches('.').to_string()).filter(|d| !d.is_empty());
            Some(TsPeer {
                ip,
                dns_name,
                host_name: p.get("HostName").and_then(|h| h.as_str()).unwrap_or_default().to_string(),
                online: p.get("Online").and_then(|o| o.as_bool()).unwrap_or(false),
            })
        })
        .collect()
}

/// Online Tailscale peers; empty when Tailscale isn't installed or running.
pub async fn discover() -> Vec<TsPeer> {
    let Some(bin) = tailscale_binary() else {
        tracing::debug!("tailscale not found; no devices to discover");
        return Vec::new();
    };
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.args(["status", "--json"]).kill_on_drop(true).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    match tokio::time::timeout(TAILSCALE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => match serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            Ok(v) => parse_status(&v).into_iter().filter(|p| p.online).collect(),
            Err(e) => {
                tracing::warn!("could not read `tailscale status --json`: {e}");
                Vec::new()
            }
        },
        Ok(Err(e)) => {
            tracing::warn!("could not run {}: {e}", bin.display());
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("`tailscale status` timed out");
            Vec::new()
        }
    }
}

// ------------------------------------------------------------ wire types

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub device_id: String,
    pub device_name: String,
    pub version: u32,
}

#[derive(Debug, Deserialize)]
pub struct PairRequest {
    pub code: String,
    pub device_id: String,
    pub device_name: String,
    pub port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PairResponse {
    pub device_id: String,
    pub device_name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestTrack {
    pub video_id: String,
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub duration_sec: i64,
    #[serde(default)]
    pub thumbnail_url: String,
    #[serde(default)]
    pub play_count: i64,
    #[serde(default)]
    pub last_played: Option<i64>,
    #[serde(default)]
    pub added_at: Option<i64>,
    pub rel_path: String,
    #[serde(default)]
    pub size: u64,
}

impl ManifestTrack {
    fn track(&self) -> Track {
        Track {
            video_id: self.video_id.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            duration_sec: self.duration_sec,
            thumbnail_url: self.thumbnail_url.clone(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestPlaylist {
    pub name: String,
    pub kind: String,
    pub video_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub tracks: Vec<ManifestTrack>,
    pub playlists: Vec<ManifestPlaylist>,
}

/// Downloaded songs (file still on disk inside the music dir) and every playlist except the
/// Discover Mix; playlists only list songs that are in `tracks`.
pub fn build_manifest(db: &Db, music_dir: &Path) -> ApiResult<Manifest> {
    let mut tracks = Vec::new();
    for t in db.library_tracks(None, None)? {
        // Only files `/sync/file` will serve (inside the music dir).
        let Some(file) = t.file_path.as_deref().and_then(|p| file_inside(Path::new(p), music_dir)) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&file) else { continue };
        let file = file.as_path();
        tracks.push(ManifestTrack {
            rel_path: rel_path_for(file, music_dir),
            size: meta.len(),
            video_id: t.video_id,
            title: t.title,
            artist: t.artist,
            album: t.album,
            duration_sec: t.duration_sec,
            thumbnail_url: t.thumbnail_url,
            play_count: t.play_count,
            last_played: t.last_played,
            added_at: t.added_at,
        });
    }
    let have: std::collections::HashSet<&str> = tracks.iter().map(|t| t.video_id.as_str()).collect();
    let playlists = db
        .playlists_for_sync()?
        .into_iter()
        .map(|(name, kind, ids)| ManifestPlaylist {
            name,
            kind,
            video_ids: ids.into_iter().filter(|id| have.contains(id.as_str())).collect(),
        })
        .collect();
    Ok(Manifest { tracks, playlists })
}

/// Progress for the app, sent as SSE event `sync`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SyncEvent {
    /// "started", "progress", "done" or "error".
    pub state: String,
    pub device: String,
    pub done: usize,
    pub total: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceOut {
    pub id: String,
    pub name: String,
    pub address: String,
    pub online: bool,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
}

impl DeviceOut {
    pub fn new(p: &Peer, online: bool) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            address: p.address.clone(),
            online,
            last_sync_at: p.last_sync_at.map(iso_time),
            last_error: p.last_error.clone(),
        }
    }
}

/// What one pull from a peer did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PullReport {
    pub downloaded: usize,
    pub failed: usize,
    pub playlist_songs_added: usize,
    pub last_error: Option<String>,
}

// ------------------------------------------------------------ manager

/// Clears the "syncing" flag when the sync ends (also on panic).
pub struct SyncGuard(Arc<AtomicBool>);

impl Drop for SyncGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct SyncManager {
    db: Arc<Db>,
    music_dir: PathBuf,
    downloads: DownloadManager,
    client: reqwest::Client,
    /// Port of our own peer listener (sent when pairing; also the port we
    /// assume for devices found through Tailscale).
    port: u16,
    events: broadcast::Sender<SyncEvent>,
    syncing: Arc<AtomicBool>,
    limiter: Arc<Mutex<PairLimiter>>,
}

impl SyncManager {
    pub fn new(db: Arc<Db>, music_dir: PathBuf, downloads: DownloadManager, port: u16) -> ApiResult<Self> {
        ensure_identity(&db)?;
        let client = reqwest::Client::builder()
            .user_agent(format!("Geonix-sync/{PROTOCOL_VERSION}"))
            .connect_timeout(PAIR_TIMEOUT)
            .read_timeout(Duration::from_secs(30))
            .no_proxy()
            .build()
            .map_err(|e| ApiError::internal(format!("http client: {e}")))?;
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            db,
            music_dir,
            downloads,
            client,
            port,
            events,
            syncing: Arc::new(AtomicBool::new(false)),
            limiter: Arc::new(Mutex::new(PairLimiter::default())),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    pub fn is_syncing(&self) -> bool {
        self.syncing.load(Ordering::SeqCst)
    }

    pub fn music_dir(&self) -> &Path {
        &self.music_dir
    }

    fn emit(&self, state: &str, device: &str, done: usize, total: usize, message: impl Into<String>) {
        let _ = self.events.send(SyncEvent { state: state.into(), device: device.into(), done, total, message: message.into() });
    }

    fn try_begin(&self) -> Option<SyncGuard> {
        self.syncing.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).ok().map(|_| SyncGuard(self.syncing.clone()))
    }

    fn auth(&self, req: reqwest::RequestBuilder, peer: &Peer) -> ApiResult<reqwest::RequestBuilder> {
        let me = this_device(&self.db)?;
        Ok(req.bearer_auth(&peer.token).header(DEVICE_HEADER, me.id))
    }

    // -------------------------------------------------------------- hello

    pub async fn hello(&self, address: &str) -> Option<Hello> {
        let resp = self.client.get(format!("http://{address}/sync/hello")).timeout(HELLO_TIMEOUT).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Hello>().await.ok()
    }

    pub async fn is_online(&self, peer: &Peer) -> bool {
        self.hello(&peer.address).await.is_some_and(|h| h.device_id == peer.id)
    }

    /// The peer with a working address: the stored one, else one found again
    /// through Tailscale (by DNS name, then by asking every online peer).
    pub async fn reachable(&self, peer: &Peer) -> Option<Peer> {
        if self.is_online(peer).await {
            return Some(peer.clone());
        }
        let port = port_of(&peer.address).unwrap_or(self.port);
        let online = discover().await;
        let found_by_dns = peer
            .dns_name
            .as_deref()
            .and_then(|dns| online.iter().find(|p| p.dns_name.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(dns))));
        let mut candidates: Vec<&TsPeer> = found_by_dns.into_iter().collect();
        candidates.extend(online.iter().filter(|p| Some(*p) != found_by_dns));
        let mut checks: FuturesUnordered<_> = candidates
            .into_iter()
            .map(|ts| async move {
                let address = SocketAddr::new(ts.ip, port).to_string();
                let hello = self.hello(&address).await;
                (address, ts.dns_name.clone(), hello)
            })
            .collect();
        while let Some((address, dns, hello)) = checks.next().await {
            if hello.is_some_and(|h| h.device_id == peer.id) {
                tracing::info!("sync: {} moved to {address}", peer.name);
                if let Err(e) = self.db.set_peer_address(&peer.id, &address, dns.as_deref()) {
                    tracing::warn!("saving new address of {}: {e}", peer.name);
                }
                let mut p = peer.clone();
                p.address = address;
                p.dns_name = dns.or(p.dns_name);
                return Some(p);
            }
        }
        None
    }

    // -------------------------------------------------------------- pairing

    /// Local side: find the device whose code this is and pair with it.
    pub async fn pair(&self, code: &str, address: Option<&str>) -> ApiResult<Peer> {
        let code = normalize_code(code).ok_or_else(|| ApiError::bad_request("A pairing code is 6 letters (A-Z, without I and O)"))?;
        let me = this_device(&self.db)?;
        if code == me.code {
            return Err(ApiError::bad_request("That is this device's own code; type the code shown on the other device"));
        }
        let candidates: Vec<(String, Option<String>)> = match address.map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => vec![(normalize_address(a, self.port), None)],
            None => discover().await.into_iter().map(|p| (SocketAddr::new(p.ip, self.port).to_string(), p.dns_name)).collect(),
        };
        let body = json!({
            "code": code,
            "device_id": me.id,
            "device_name": me.name,
            "port": self.port,
        });
        let mut attempts: FuturesUnordered<_> = candidates
            .into_iter()
            .map(|(address, dns)| {
                let req = self.client.post(format!("http://{address}/sync/pair")).timeout(PAIR_TIMEOUT).json(&body);
                async move {
                    let res = async {
                        let resp = req.send().await?;
                        let status = resp.status();
                        if status.is_success() {
                            Ok((status, Some(resp.json::<PairResponse>().await?)))
                        } else {
                            Ok::<_, reqwest::Error>((status, None))
                        }
                    }
                    .await;
                    (address, dns, res)
                }
            })
            .collect();
        let mut limited = false;
        while let Some((address, dns, res)) = attempts.next().await {
            match res {
                Ok((_, Some(accepted))) if !accepted.device_id.is_empty() && !accepted.token.is_empty() => {
                    let peer = Peer {
                        id: accepted.device_id,
                        name: clean_name(&accepted.device_name),
                        address,
                        dns_name: dns,
                        token: accepted.token,
                        last_sync_at: None,
                        last_error: None,
                    };
                    self.db.upsert_peer(&peer)?;
                    tracing::info!("sync: paired with {} at {}", peer.name, peer.address);
                    return Ok(self.db.get_peer(&peer.id)?.unwrap_or(peer));
                }
                Ok((StatusCode::TOO_MANY_REQUESTS, _)) => limited = true,
                Ok(_) => {}
                Err(e) => tracing::debug!("sync: pairing via {address}: {e}"),
            }
        }
        if limited {
            return Err(ApiError::status(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many pairing attempts on that device; wait a minute and try again",
            ));
        }
        Err(ApiError::not_found(NO_DEVICE_WITH_CODE))
    }

    /// Peer side: a device typed our code.
    pub fn accept_pair(&self, remote: IpAddr, req: PairRequest) -> ApiResult<PairResponse> {
        let now = Instant::now();
        {
            let mut limiter = self.limiter.lock().unwrap_or_else(|p| p.into_inner());
            if limiter.blocked(now) {
                return Err(ApiError::status(StatusCode::TOO_MANY_REQUESTS, "too many pairing attempts"));
            }
        }
        let me = this_device(&self.db)?;
        let typed = normalize_code(&req.code).unwrap_or_default();
        if !ct_eq(typed.as_bytes(), me.code.as_bytes()) {
            self.limiter.lock().unwrap_or_else(|p| p.into_inner()).fail(now);
            return Err(ApiError::status(StatusCode::FORBIDDEN, "wrong code"));
        }
        let device_id = req.device_id.trim();
        if device_id.is_empty() || device_id.len() > 64 || device_id == me.id || req.port == 0 {
            return Err(ApiError::bad_request("invalid pairing request"));
        }
        let remote = canonical_ip(remote);
        let peer = Peer {
            id: device_id.to_string(),
            name: clean_name(&req.device_name),
            address: SocketAddr::new(remote, req.port).to_string(),
            dns_name: None,
            token: generate_token(),
            last_sync_at: None,
            last_error: None,
        };
        self.db.upsert_peer(&peer)?;
        rotate_code(&self.db)?;
        tracing::info!("sync: {} paired with this device from {}", peer.name, peer.address);
        if !remote.is_loopback() {
            // Remember its Tailscale DNS name so it can be found if its IP changes.
            let db = self.db.clone();
            let id = peer.id.clone();
            let address = peer.address.clone();
            tokio::spawn(async move {
                if let Some(ts) = discover().await.into_iter().find(|p| p.ip == remote) {
                    if let Err(e) = db.set_peer_address(&id, &address, ts.dns_name.as_deref()) {
                        tracing::warn!("saving DNS name of paired device: {e}");
                    }
                }
            });
        }
        Ok(PairResponse { device_id: me.id, device_name: me.name, token: peer.token })
    }

    /// Peer side: check `Authorization: Bearer` + `X-Geonix-Device`.
    pub fn authenticate(&self, headers: &HeaderMap, remote: IpAddr) -> ApiResult<Peer> {
        let unauthorized = || ApiError::status(StatusCode::UNAUTHORIZED, "not paired with this device");
        let device = headers.get(DEVICE_HEADER).and_then(|v| v.to_str().ok()).ok_or_else(unauthorized)?;
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(unauthorized)?;
        let peer = self.db.get_peer(device.trim())?.ok_or_else(unauthorized)?;
        if !ct_eq(token.trim().as_bytes(), peer.token.as_bytes()) {
            return Err(unauthorized());
        }
        // Keep the stored address current when the peer's IP changed.
        let remote = canonical_ip(remote);
        if let Ok(stored) = peer.address.parse::<SocketAddr>() {
            if stored.ip() != remote {
                let address = SocketAddr::new(remote, stored.port()).to_string();
                if let Err(e) = self.db.set_peer_address(&peer.id, &address, None) {
                    tracing::warn!("updating address of {}: {e}", peer.name);
                }
            }
        }
        Ok(peer)
    }

    /// Local side: forget a device here and (best effort) on the device.
    pub async fn unpair(&self, id: &str) -> ApiResult<()> {
        let peer = self.db.get_peer(id)?.ok_or_else(|| ApiError::not_found("device not found"))?;
        self.db.delete_peer(id)?;
        let req = self.auth(self.client.delete(format!("http://{}/sync/pair", peer.address)), &peer)?;
        tokio::spawn(async move {
            if let Err(e) = req.timeout(PAIR_TIMEOUT).send().await {
                tracing::debug!("sync: telling {} to forget us failed: {e}", peer.name);
            }
        });
        Ok(())
    }

    // -------------------------------------------------------------- syncing

    /// Start a background sync with all paired devices (or one).
    pub fn start_run(&self, device_id: Option<String>) -> ApiResult<()> {
        if let Some(id) = &device_id {
            self.db.get_peer(id)?.ok_or_else(|| ApiError::not_found("device not found"))?;
        }
        let guard = self.try_begin().ok_or_else(|| ApiError::status(StatusCode::CONFLICT, "A sync is already running"))?;
        let this = self.clone();
        tokio::spawn(async move { this.run(guard, device_id).await });
        Ok(())
    }

    async fn run(&self, _guard: SyncGuard, device_id: Option<String>) {
        let peers: Vec<Peer> = match self.db.peers() {
            Ok(p) => p.into_iter().filter(|p| device_id.as_ref().is_none_or(|id| *id == p.id)).collect(),
            Err(e) => {
                self.emit("error", "", 0, 0, format!("database error: {e}"));
                self.emit("done", "", 0, 0, "Sync failed");
                return;
            }
        };
        let names = peers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        self.emit("started", &names, 0, 0, "Syncing");
        // Every device that was tried gets exactly one "done" event whose
        // `done` is the number of new songs pulled from it.
        let mut tried = 0usize;
        for peer in &peers {
            let Some(peer) = self.reachable(peer).await else {
                if device_id.is_some() {
                    let msg = format!("{} is offline", peer.name);
                    let _ = self.db.set_peer_sync_result(&peer.id, Some(&msg));
                    self.emit("error", &peer.name, 0, 0, msg.clone());
                    self.emit("done", &peer.name, 0, 0, msg);
                    tried += 1;
                }
                continue;
            };
            tried += 1;
            match self.pull_from(&peer).await {
                Ok(report) => {
                    let _ = self.db.set_peer_sync_result(&peer.id, None);
                    if report.failed > 0 {
                        let msg =
                            format!("{} song(s) could not be copied: {}", report.failed, report.last_error.clone().unwrap_or_default());
                        let _ = self.db.set_peer_sync_result(&peer.id, Some(&msg));
                        self.emit("error", &peer.name, report.downloaded, report.downloaded + report.failed, msg);
                    }
                    if let Err(e) = self.request_pull(&peer).await {
                        tracing::warn!("sync: asking {} to pull from us: {e}", peer.name);
                    }
                    let n = report.downloaded;
                    self.emit("done", &peer.name, n, n, songs_message(n));
                }
                Err(e) => {
                    let _ = self.db.set_peer_sync_result(&peer.id, Some(&e));
                    self.emit("error", &peer.name, 0, 0, e);
                    self.emit("done", &peer.name, 0, 0, "Sync failed");
                }
            }
        }
        if tried == 0 {
            let msg = if peers.is_empty() { "No paired devices" } else { "No paired devices are online" };
            self.emit("done", "", 0, 0, msg);
        }
    }

    /// Peer side of `POST /sync/request`: pull from the caller in the
    /// background (and don't ask it back, which would loop).
    pub fn start_incoming(&self, peer: Peer) -> ApiResult<()> {
        let guard = self.try_begin().ok_or_else(|| ApiError::status(StatusCode::CONFLICT, "A sync is already running"))?;
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            this.emit("started", &peer.name, 0, 0, "Syncing");
            match this.pull_from(&peer).await {
                Ok(report) => {
                    let _ = this.db.set_peer_sync_result(&peer.id, None);
                    let n = report.downloaded;
                    this.emit("done", &peer.name, n, n, songs_message(n));
                }
                Err(e) => {
                    let _ = this.db.set_peer_sync_result(&peer.id, Some(&e));
                    this.emit("error", &peer.name, 0, 0, e);
                    this.emit("done", &peer.name, 0, 0, "Sync failed");
                }
            }
        });
        Ok(())
    }

    async fn request_pull(&self, peer: &Peer) -> Result<(), String> {
        let req = self.auth(self.client.post(format!("http://{}/sync/request", peer.address)), peer).map_err(|e| e.to_string())?;
        let resp = req.timeout(PAIR_TIMEOUT * 2).send().await.map_err(|e| e.to_string())?;
        match resp.status() {
            s if s.is_success() || s == StatusCode::CONFLICT => Ok(()),
            s => Err(format!("{} answered {s}", peer.name)),
        }
    }

    fn has_local_file(&self, video_id: &str) -> bool {
        matches!(self.db.get_track(video_id), Ok(Some(t))
            if t.file_path.as_deref().is_some_and(|p| Path::new(p).is_file()))
    }

    /// Copy every song we don't have from the peer, then merge playlists.
    pub async fn pull_from(&self, peer: &Peer) -> Result<PullReport, String> {
        let req = self.auth(self.client.get(format!("http://{}/sync/manifest", peer.address)), peer).map_err(|e| e.to_string())?;
        let resp = req.timeout(MANIFEST_TIMEOUT).send().await.map_err(|e| format!("{} is unreachable: {e}", peer.name))?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(format!("{} no longer knows this device; pair again", peer.name));
        }
        if !resp.status().is_success() {
            return Err(format!("{} answered {}", peer.name, resp.status()));
        }
        let manifest: Manifest = resp.json().await.map_err(|e| format!("bad answer from {}: {e}", peer.name))?;

        let missing: Vec<&ManifestTrack> = manifest
            .tracks
            .iter()
            .filter(|t| crate::routes::tracks::check_id(&t.video_id).is_ok() && !self.has_local_file(&t.video_id))
            .collect();
        let total = missing.len();
        let mut report = PullReport::default();
        self.emit("progress", &peer.name, 0, total, format!("{total} new song(s) on {}", peer.name));
        for (i, t) in missing.into_iter().enumerate() {
            match self.fetch_track(peer, t).await {
                Ok(()) => report.downloaded += 1,
                Err(e) => {
                    tracing::warn!("sync: copying {} from {} failed: {e}", t.video_id, peer.name);
                    report.failed += 1;
                    report.last_error = Some(e);
                }
            }
            self.emit("progress", &peer.name, i + 1, total, t.title.clone());
        }

        for pl in &manifest.playlists {
            let name = pl.name.trim();
            if pl.kind == "discover" || name.is_empty() {
                continue;
            }
            let name: String = name.chars().take(120).collect();
            match self.db.merge_playlist(&name, &pl.kind, &pl.video_ids) {
                Ok((added, _)) => report.playlist_songs_added += added,
                Err(e) => tracing::warn!("sync: merging playlist {name}: {e}"),
            }
        }
        if report.downloaded > 0 || report.playlist_songs_added > 0 {
            self.downloads.library_changed();
        }
        tracing::info!(
            "sync: pulled from {}: {} new song(s), {} failed, {} playlist entr(ies) added",
            peer.name,
            report.downloaded,
            report.failed,
            report.playlist_songs_added
        );
        Ok(report)
    }

    /// Download one song to `music_dir/rel_path` (temp file + rename, never
    /// overwriting) and add it to the library.
    async fn fetch_track(&self, peer: &Peer, t: &ManifestTrack) -> Result<(), String> {
        let rel = sanitize_rel_path(&t.rel_path).unwrap_or_else(|| PathBuf::from(format!("{}.mp3", t.video_id)));
        let mut dest = self.music_dir.join(&rel);
        let same_file = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && (t.size == 0 || m.len() == t.size));
        if dest.exists() {
            if same_file(&dest) {
                // Already there (e.g. copied before the library forgot it).
                return self.link(t, &dest);
            }
            let stem = dest.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let name = match dest.extension() {
                Some(ext) => format!("{stem} [{}].{}", t.video_id, ext.to_string_lossy()),
                None => format!("{stem} [{}]", t.video_id),
            };
            dest.set_file_name(name);
            if dest.exists() {
                return if same_file(&dest) { self.link(t, &dest) } else { Err(format!("{} already exists", dest.display())) };
            }
        }
        let parent = dest.parent().ok_or("bad destination")?.to_path_buf();
        tokio::fs::create_dir_all(&parent).await.map_err(|e| format!("creating {}: {e}", parent.display()))?;
        let inside = match (parent.canonicalize(), self.music_dir.canonicalize()) {
            (Ok(p), Ok(m)) => p.starts_with(m),
            _ => false,
        };
        if !inside {
            return Err(format!("{} is outside the music folder", dest.display()));
        }

        let file_name = dest.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let tmp = parent.join(format!(".{file_name}.{}.part", uuid::Uuid::new_v4().simple()));
        let result = self.download_to(peer, t, &tmp).await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        if dest.exists() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("{} appeared while copying", dest.display()));
        }
        if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("saving {}: {e}", dest.display()));
        }
        self.link(t, &dest)
    }

    fn link(&self, t: &ManifestTrack, file: &Path) -> Result<(), String> {
        self.db.upsert_synced_track(&t.track(), &file.to_string_lossy()).map_err(|e| format!("saving to library: {e}"))
    }

    async fn download_to(&self, peer: &Peer, t: &ManifestTrack, tmp: &Path) -> Result<(), String> {
        let req =
            self.auth(self.client.get(format!("http://{}/sync/file/{}", peer.address, t.video_id)), peer).map_err(|e| e.to_string())?;
        let mut resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("{} answered {}", peer.name, resp.status()));
        }
        let mut file = tokio::fs::File::create(tmp).await.map_err(|e| format!("creating {}: {e}", tmp.display()))?;
        let mut written: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("download interrupted: {e}"))? {
            file.write_all(&chunk).await.map_err(|e| format!("writing: {e}"))?;
            written += chunk.len() as u64;
            if t.size > 0 && written > t.size {
                // Don't let a misbehaving peer fill the disk.
                return Err(format!("{} sent more than the announced {} bytes", peer.name, t.size));
            }
        }
        file.flush().await.map_err(|e| format!("writing: {e}"))?;
        file.sync_all().await.map_err(|e| format!("writing: {e}"))?;
        if t.size > 0 && written != t.size {
            return Err(format!("incomplete download ({written} of {} bytes)", t.size));
        }
        Ok(())
    }
}

fn songs_message(n: usize) -> String {
    match n {
        0 => "Sync finished, no new songs".into(),
        1 => "Sync finished, 1 new song".into(),
        n => format!("Sync finished, {n} new songs"),
    }
}

fn clean_name(name: &str) -> String {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    if name.is_empty() {
        "Unnamed device".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_alphabet_and_vary() {
        assert_eq!(CODE_ALPHABET.len(), 24);
        assert!(!CODE_ALPHABET.contains(&b'I') && !CODE_ALPHABET.contains(&b'O'));
        let mut seen = std::collections::HashSet::new();
        let mut letters = std::collections::HashSet::new();
        for _ in 0..500 {
            let c = generate_code();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{c}");
            assert_eq!(normalize_code(&c).as_deref(), Some(c.as_str()));
            letters.extend(c.bytes());
            seen.insert(c);
        }
        assert!(seen.len() > 490, "codes must be random");
        assert_eq!(letters.len(), 24, "every letter shows up");
    }

    #[test]
    fn typed_codes_are_normalized() {
        assert_eq!(normalize_code(" abc-def ").as_deref(), Some("ABCDEF"));
        assert_eq!(normalize_code("abcdeI"), None, "I is not in the alphabet");
        assert_eq!(normalize_code("ABCDE0"), None);
        assert_eq!(normalize_code("ABCDE"), None);
        assert_eq!(normalize_code("ABCDEFG"), None);
    }

    #[test]
    fn tokens_are_32_random_bytes_hex() {
        let a = generate_token();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, generate_token());
        assert!(ct_eq(a.as_bytes(), a.clone().as_bytes()));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }

    #[test]
    fn only_tailscale_and_loopback_may_connect() {
        for ok in [
            "100.64.0.1",
            "100.127.255.254",
            "100.100.100.100",
            "127.0.0.1",
            "::1",
            "fd7a:115c:a1e0::1",
            "fd7a:115c:a1e0:ab12::42",
            "::ffff:100.64.1.2",
        ] {
            assert!(is_allowed_peer_ip(ok.parse().unwrap()), "{ok} should be allowed");
        }
        for bad in [
            "100.63.255.255",
            "100.128.0.1",
            "192.168.1.10",
            "10.0.0.1",
            "8.8.8.8",
            "0.0.0.0",
            "fd7a:115c:a1e1::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(!is_allowed_peer_ip(bad.parse().unwrap()), "{bad} must be rejected");
        }
    }

    #[test]
    fn rel_paths_are_sanitized() {
        assert_eq!(sanitize_rel_path("Artist/Song.mp3"), Some(PathBuf::from("Artist").join("Song.mp3")));
        assert_eq!(sanitize_rel_path("Artist\\Song.mp3"), Some(PathBuf::from("Artist").join("Song.mp3")));
        assert_eq!(sanitize_rel_path("./a//b.mp3"), Some(PathBuf::from("a").join("b.mp3")));
        assert_eq!(sanitize_rel_path("AC/DC: Live?.mp3"), Some(PathBuf::from("AC").join("DC_ Live_.mp3")));
        for bad in ["", "/etc/passwd", "\\\\server\\x", "C:\\x.mp3", "c:x.mp3", "../x.mp3", "a/../../x", "a/..", "...", " / "] {
            assert_eq!(sanitize_rel_path(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn rel_path_is_relative_to_music_dir() {
        let music = Path::new("/home/u/Music/Geonix");
        assert_eq!(rel_path_for(&music.join("A").join("b.mp3"), music), "A/b.mp3");
        assert_eq!(rel_path_for(Path::new("/elsewhere/c.mp3"), music), "c.mp3");
    }

    #[test]
    fn rel_paths_cross_between_windows_and_unix() {
        // Sender side always uses '/', whatever the OS separator is.
        let music = std::env::temp_dir().join("geonix-music");
        let file = music.join("Some Artist").join("Album").join("Song.mp3");
        let rel = rel_path_for(&file, &music);
        assert_eq!(rel, "Some Artist/Album/Song.mp3");
        // Receiver joins the parts with its own separator, from either form.
        let want: PathBuf = ["Some Artist", "Album", "Song.mp3"].iter().collect();
        assert_eq!(sanitize_rel_path(&rel), Some(want.clone()));
        assert_eq!(sanitize_rel_path("Some Artist\\Album\\Song.mp3"), Some(want.clone()));
        assert_eq!(music.join(sanitize_rel_path(&rel).unwrap()), file);
    }

    #[test]
    fn file_inside_rejects_escapes() {
        let dir = std::env::temp_dir().join(format!("geonix-inside-{}", uuid::Uuid::new_v4()));
        let music = dir.join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("a.mp3"), b"x").unwrap();
        std::fs::write(dir.join("secret"), b"x").unwrap();
        assert!(file_inside(&music.join("a.mp3"), &music).is_some());
        assert!(file_inside(&music.join("..").join("secret"), &music).is_none());
        assert!(file_inside(&dir.join("secret"), &music).is_none());
        assert!(file_inside(&music.join("missing.mp3"), &music).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn pair_limiter_blocks_after_ten_failures_for_the_minute() {
        let mut l = PairLimiter::default();
        let t0 = Instant::now();
        for i in 0..10 {
            assert!(!l.blocked(t0 + Duration::from_secs(i)));
            l.fail(t0 + Duration::from_secs(i));
        }
        assert!(l.blocked(t0 + Duration::from_secs(30)));
        assert!(l.blocked(t0 + Duration::from_secs(59)));
        assert!(!l.blocked(t0 + Duration::from_secs(61)), "a new minute starts fresh");
        l.fail(t0 + Duration::from_secs(61));
        assert!(!l.blocked(t0 + Duration::from_secs(62)));
    }

    #[test]
    fn addresses_get_a_port() {
        assert_eq!(normalize_address("127.0.0.1:7813", 7803), "127.0.0.1:7813");
        assert_eq!(normalize_address("100.64.0.5", 7803), "100.64.0.5:7803");
        assert_eq!(normalize_address("fd7a:115c:a1e0::5", 7803), "[fd7a:115c:a1e0::5]:7803");
        assert_eq!(normalize_address("laptop.tail.ts.net", 7803), "laptop.tail.ts.net:7803");
        assert_eq!(normalize_address("http://laptop:9000/", 7803), "laptop:9000");
        assert_eq!(port_of("[::1]:7813"), Some(7813));
    }

    #[test]
    fn iso_times() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_time(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso_time(1_792_406_096_789), "2026-10-19T10:34:56Z");
    }

    #[test]
    fn tailscale_status_is_parsed() {
        let v = json!({
            "Self": {"HostName": "me", "TailscaleIPs": ["100.64.0.1"], "Online": true},
            "Peer": {
                "nodekey:a": {"HostName": "laptop", "DNSName": "laptop.tail1.ts.net.", "Online": true,
                              "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"]},
                "nodekey:b": {"HostName": "phone", "DNSName": "", "Online": false, "TailscaleIPs": ["100.64.0.3"]},
                "nodekey:c": {"HostName": "broken", "TailscaleIPs": []}
            }
        });
        let mut peers = parse_status(&v);
        peers.sort_by_key(|p| p.host_name.clone());
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].host_name, "laptop");
        assert_eq!(peers[0].ip, "100.64.0.2".parse::<IpAddr>().unwrap());
        assert_eq!(peers[0].dns_name.as_deref(), Some("laptop.tail1.ts.net"));
        assert!(peers[0].online);
        assert!(!peers[1].online);
        assert_eq!(peers[1].dns_name, None);
        assert!(parse_status(&json!({})).is_empty());
    }

    #[test]
    fn identity_is_created_once_and_code_rotates() {
        let db = Db::open_in_memory().unwrap();
        ensure_identity(&db).unwrap();
        let a = this_device(&db).unwrap();
        assert_eq!(a.id.len(), 36);
        assert!(normalize_code(&a.code).is_some());
        assert!(!a.name.is_empty());
        ensure_identity(&db).unwrap();
        assert_eq!(this_device(&db).unwrap(), a, "restarts keep the identity");
        let b = rotate_code(&db).unwrap();
        assert_eq!(b.id, a.id);
        assert_ne!(b.code, a.code);
    }
}
