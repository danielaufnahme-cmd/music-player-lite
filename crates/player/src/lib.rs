//! Geonix LITE audio engine. See README.md for the API overview.
//!
//! One engine thread owns the decoder and the output stream; [`Player`]
//! methods only post commands to it, so they never block the UI thread.

mod convert;
#[doc(hidden)]
pub mod decode;
mod engine;
mod media;
mod output;
mod source;

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub use output::{CpalBackend, NullBackend, NullStats, OutputBackend, OutputStream, Renderer, StreamSpec};
pub use source::{HttpReader, Source};

/// Playback state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PlaybackState {
    /// Nothing loaded (initial state, after `stop()`).
    Idle = 0,
    /// Opening a track (`load()` in progress).
    Loading = 1,
    Playing = 2,
    Paused = 3,
    /// Track finished (`Ended`) or failed to load/decode (`Error`).
    Stopped = 4,
}

impl PlaybackState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Loading,
            2 => Self::Playing,
            3 => Self::Paused,
            4 => Self::Stopped,
            _ => Self::Idle,
        }
    }
}

/// OS media-key actions the player can't handle itself (the queue lives in the UI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKey {
    Next,
    Previous,
    /// "Raise" the player window (MPRIS).
    Raise,
    /// "Quit" requested from the OS (MPRIS).
    Quit,
}

/// Events from the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    /// The playback state changed.
    State(PlaybackState),
    /// The track passed to `load()` was opened.
    Loaded { duration: Option<f64> },
    /// Position update: at most 4 Hz and only while playing; also once after a seek / pause.
    Position { position: f64, duration: Option<f64> },
    /// The current track ended and the preloaded one is now playing (gapless).
    AdvancedToPreload { duration: Option<f64> },
    /// The current track ended and nothing was preloaded. State is now `Stopped`.
    Ended,
    /// Load / decode / network / device error.
    Error(String),
    /// OS media key the UI must handle.
    MediaKey(MediaKey),
}

/// What the OS "now playing" widget shows (MPRIS / macOS / SMTC).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// `http(s)://` or `file://` URL of the cover image.
    pub cover_url: Option<String>,
    pub duration: Option<f64>,
}

/// Construction options.
pub struct PlayerOptions {
    /// Where audio goes. Default: [`CpalBackend`] (system default device).
    pub backend: Box<dyn OutputBackend>,
    /// Register OS media keys / now-playing (needs feature `media-controls`).
    pub media_controls: bool,
    /// Release the output device after this long paused / ended.
    pub idle_release: Duration,
    /// Name shown by the OS media widget.
    pub display_name: String,
    /// MPRIS bus name suffix (`org.mpris.MediaPlayer2.<dbus_name>`).
    pub dbus_name: String,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        PlayerOptions {
            backend: Box::new(CpalBackend),
            media_controls: true,
            idle_release: Duration::from_secs(30),
            display_name: "Music Player Lite".into(),
            dbus_name: "geonix_lite".into(),
        }
    }
}

impl PlayerOptions {
    /// Silent options for tests: null sink, no media controls.
    pub fn null(backend: NullBackend) -> Self {
        PlayerOptions { backend: Box::new(backend), media_controls: false, ..Default::default() }
    }
}

pub(crate) enum Cmd {
    Load(Source, bool),
    Preload(Option<Source>),
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(f64),
    SeekBy(f64),
    Volume(f32),
    Meta(MediaMeta),
    Key(MediaKey),
    Opened { gen: u64, preload: bool, result: Result<Box<decode::TrackDecoder>, String> },
    Shutdown,
}

/// Lock-free snapshot the UI thread reads.
pub(crate) struct Status {
    state: AtomicU8,
    /// Position (s, f64 bits) at sample counter `seg_start`.
    base: AtomicU64,
    seg_start: AtomicU64,
    /// Output samples per second (rate × channels).
    sps: AtomicU64,
    /// Duration (f64 bits; NaN = unknown).
    dur: AtomicU64,
    vol: AtomicU32,
}

impl Status {
    fn new() -> Self {
        Status {
            state: AtomicU8::new(0),
            base: AtomicU64::new(0f64.to_bits()),
            seg_start: AtomicU64::new(0),
            sps: AtomicU64::new(88200),
            dur: AtomicU64::new(f64::NAN.to_bits()),
            vol: AtomicU32::new(1f32.to_bits()),
        }
    }

    pub(crate) fn state(&self) -> PlaybackState {
        PlaybackState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn duration(&self) -> Option<f64> {
        let d = f64::from_bits(self.dur.load(Ordering::Acquire));
        (!d.is_nan()).then_some(d)
    }

    pub(crate) fn position(&self, consumed: u64) -> f64 {
        if matches!(self.state(), PlaybackState::Idle | PlaybackState::Loading) {
            return 0.0;
        }
        let base = f64::from_bits(self.base.load(Ordering::Acquire));
        let seg = self.seg_start.load(Ordering::Acquire);
        let sps = self.sps.load(Ordering::Acquire).max(1);
        let p = base + consumed.saturating_sub(seg) as f64 / sps as f64;
        match self.duration() {
            Some(d) => p.min(d),
            None => p,
        }
    }
}

struct Inner {
    tx: Sender<Cmd>,
    status: Arc<Status>,
    shared: Arc<output::Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

/// Handle to the audio engine. Cheap to clone; all methods are non-blocking.
/// The engine shuts down when the last clone is dropped.
#[derive(Clone)]
pub struct Player {
    inner: Arc<Inner>,
}

impl Player {
    /// System audio output + OS media controls.
    pub fn new() -> (Player, Receiver<PlayerEvent>) {
        Self::with_options(PlayerOptions::default())
    }

    pub fn with_options(opts: PlayerOptions) -> (Player, Receiver<PlayerEvent>) {
        let (tx, rx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let status = Arc::new(Status::new());
        let shared = output::Shared::new();
        let (tx2, st2, sh2) = (tx.clone(), status.clone(), shared.clone());
        let thread = std::thread::Builder::new()
            .name("geonix-player".into())
            .spawn(move || engine::Engine::new(opts, tx2, etx, st2, sh2).run(rx))
            .expect("spawn player thread");
        let inner = Inner { tx, status, shared, thread: Mutex::new(Some(thread)) };
        (Player { inner: Arc::new(inner) }, erx)
    }

    fn send(&self, c: Cmd) {
        let _ = self.inner.tx.send(c);
    }

    /// Open `source` (dropping the current track and any preload); play when `autoplay`.
    pub fn load(&self, source: Source, autoplay: bool) {
        self.send(Cmd::Load(source, autoplay));
    }

    /// Open the next track in the background so it starts gaplessly when the
    /// current one ends (`AdvancedToPreload`). `None` clears the preload.
    pub fn preload(&self, source: Option<Source>) {
        self.send(Cmd::Preload(source));
    }

    pub fn play(&self) {
        self.send(Cmd::Play);
    }

    pub fn pause(&self) {
        self.send(Cmd::Pause);
    }

    pub fn toggle(&self) {
        self.send(Cmd::Toggle);
    }

    /// Unload everything and release the device. State becomes `Idle`.
    pub fn stop(&self) {
        self.send(Cmd::Stop);
    }

    /// Seek to `secs` in the current track.
    pub fn seek(&self, secs: f64) {
        self.send(Cmd::Seek(secs));
    }

    /// 0.0..=1.0, perceptual (cubic) curve like mpv's volume.
    pub fn set_volume(&self, volume: f32) {
        let v = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
        self.inner.status.vol.store(v.to_bits(), Ordering::Release);
        self.send(Cmd::Volume(v));
    }

    /// Update the OS now-playing info.
    pub fn set_metadata(&self, meta: MediaMeta) {
        self.send(Cmd::Meta(meta));
    }

    /// Current position in seconds.
    pub fn position(&self) -> f64 {
        let consumed = self.inner.shared.consumed.load(Ordering::Acquire);
        self.inner.status.position(consumed)
    }

    pub fn duration(&self) -> Option<f64> {
        self.inner.status.duration()
    }

    pub fn state(&self) -> PlaybackState {
        self.inner.status.state()
    }

    pub fn is_playing(&self) -> bool {
        self.state() == PlaybackState::Playing
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.inner.status.vol.load(Ordering::Acquire))
    }
}
