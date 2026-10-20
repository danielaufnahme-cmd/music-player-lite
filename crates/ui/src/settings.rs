//! Small per-user UI settings, saved as JSON in ~/.config/geonix-lite/ui.json
//! (GEONIX_UI_SETTINGS overrides the path).
//!
//! ## Read API for other modules (perf / visuals / waves)
//! ```ignore
//! let v = crate::settings::current();   // cheap Copy snapshot, any thread
//! v.waves_enabled    // bool: animated waves wanted (already false when reduce_motion)
//! v.aurora_enabled   // bool: background aurora animation wanted (false when reduce_motion)
//! v.cover_glow       // bool: glow behind the cover
//! v.reduce_motion    // bool: user asked for minimal animation
//! v.max_fps          // u32: frame-rate cap for animations (12 / 24 / 30)
//! v.gpu_renderer     // bool: use femtovg at next start (read once in main before select_backend)
//! crate::settings::generation()        // u64, bumps on every change: poll it to notice changes
//! ```
//! `current()` is updated by the Settings page whenever a toggle changes, and
//! at startup from ui.json, so readers never need the App borrow.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// Snapshot for renderer / animation code. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visual {
    pub waves_enabled: bool,
    pub aurora_enabled: bool,
    pub cover_glow: bool,
    pub reduce_motion: bool,
    pub max_fps: u32,
    pub gpu_renderer: bool,
}

impl Default for Visual {
    fn default() -> Self {
        Settings::default().visual()
    }
}

static CURRENT: RwLock<Option<Visual>> = RwLock::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static MEDIA_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Register OS media keys (read once when the audio engine starts).
pub fn media_keys() -> bool {
    MEDIA_KEYS.load(Ordering::Relaxed)
}

/// Current visual settings (defaults until the app has loaded ui.json).
#[allow(dead_code)]
pub fn current() -> Visual {
    CURRENT.read().ok().and_then(|g| *g).unwrap_or_default()
}

/// Bumps every time `current()` changes.
#[allow(dead_code)]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn publish(v: Visual) {
    if let Ok(mut g) = CURRENT.write() {
        if *g != Some(v) {
            *g = Some(v);
            GENERATION.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSource {
    #[default]
    Desktop,
    Geonix,
}

pub const FPS_CHOICES: [u32; 3] = [12, 24, 30];
pub const COVER_CACHE_CHOICES: [u32; 4] = [12, 24, 48, 96];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme_source: ThemeSource,
    pub light: bool,
    /// Last used volume (restored on start).
    pub volume: f32,

    // Appearance
    pub waves: bool,
    pub aurora: bool,
    pub cover_glow: bool,
    pub reduce_motion: bool,

    // Playback
    pub gapless: bool,
    pub autoplay_recs: bool,
    /// Volume used at start when `remember_volume` is off.
    pub default_volume: f32,
    pub remember_volume: bool,
    pub media_keys: bool,

    // Lyrics
    pub show_lyrics: bool,
    pub karaoke: bool,

    // Performance
    pub max_fps: u32,
    pub gpu_renderer: bool,
    /// MB of decoded covers kept in memory.
    pub cover_cache: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme_source: ThemeSource::Desktop,
            light: false,
            volume: 0.8,
            waves: true,
            aurora: true,
            cover_glow: true,
            reduce_motion: false,
            gapless: true,
            autoplay_recs: true,
            default_volume: 0.8,
            remember_volume: true,
            media_keys: true,
            show_lyrics: true,
            karaoke: true,
            max_fps: 30,
            gpu_renderer: false,
            cover_cache: 24,
        }
    }
}

fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("GEONIX_UI_SETTINGS").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("geonix-lite").join("ui.json")
}

impl Settings {
    pub fn load() -> Settings {
        let s: Settings = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let s = s.sanitized();
        s.apply_global();
        s
    }

    /// Clamp values to what the UI offers.
    pub fn sanitized(mut self) -> Settings {
        self.volume = if self.volume.is_finite() { self.volume.clamp(0.0, 1.0) } else { 0.8 };
        self.default_volume = if self.default_volume.is_finite() { self.default_volume.clamp(0.0, 1.0) } else { 0.8 };
        self.max_fps = self.max_fps.clamp(1, 60);
        self.cover_cache = self.cover_cache.clamp(4, 512);
        self
    }

    pub fn visual(&self) -> Visual {
        Visual {
            waves_enabled: self.waves && !self.reduce_motion,
            aurora_enabled: self.aurora && !self.reduce_motion,
            cover_glow: self.cover_glow,
            reduce_motion: self.reduce_motion,
            max_fps: self.max_fps,
            gpu_renderer: self.gpu_renderer,
        }
    }

    /// Publish this as `current()`.
    pub fn apply_global(&self) {
        MEDIA_KEYS.store(self.media_keys, Ordering::Relaxed);
        publish(self.visual());
    }

    /// Volume to start with.
    pub fn start_volume(&self) -> f32 {
        if self.remember_volume {
            self.volume
        } else {
            self.default_volume
        }
    }

    pub fn save(&self) {
        // Screenshot / benchmark runs never touch the user's settings file.
        if crate::offscreen::active() {
            return;
        }
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }
}
