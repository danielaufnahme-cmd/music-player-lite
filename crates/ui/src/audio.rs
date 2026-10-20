//! Thin wrapper over crates/player. Without the `audio` feature (or with
//! GEONIX_NULL_AUDIO=1) nothing reaches the sound card.

pub enum AudioEvent {
    Loaded(Option<f64>),
    Position(f64, Option<f64>),
    Playing(bool),
    Ended,
    /// The preloaded next track started (gapless).
    Advanced(Option<f64>),
    Error(String),
    Next,
    Previous,
    Raise,
}

pub enum Src {
    File(String),
    Url(String),
}

pub struct Meta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_url: String,
    pub duration: Option<f64>,
}

#[cfg(feature = "audio")]
pub struct Audio {
    p: geonix_player::Player,
}

#[cfg(feature = "audio")]
impl Audio {
    pub fn new(null_sink: bool, on: impl Fn(AudioEvent) + Send + 'static) -> Audio {
        use geonix_player::{MediaKey, PlaybackState, PlayerEvent, PlayerOptions};
        let opts = if null_sink {
            PlayerOptions::null(geonix_player::NullBackend::new())
        } else {
            PlayerOptions { display_name: "Music Player Lite".into(), media_controls: crate::settings::media_keys(), ..Default::default() }
        };
        let (p, rx) = geonix_player::Player::with_options(opts);
        std::thread::Builder::new()
            .name("geonix-audio-events".into())
            .spawn(move || {
                while let Ok(ev) = rx.recv() {
                    let e = match ev {
                        PlayerEvent::State(s) => AudioEvent::Playing(s == PlaybackState::Playing || s == PlaybackState::Loading),
                        PlayerEvent::Loaded { duration } => AudioEvent::Loaded(duration),
                        PlayerEvent::Position { position, duration } => AudioEvent::Position(position, duration),
                        PlayerEvent::AdvancedToPreload { duration } => AudioEvent::Advanced(duration),
                        PlayerEvent::Ended => AudioEvent::Ended,
                        PlayerEvent::Error(e) => AudioEvent::Error(e),
                        PlayerEvent::MediaKey(MediaKey::Next) => AudioEvent::Next,
                        PlayerEvent::MediaKey(MediaKey::Previous) => AudioEvent::Previous,
                        PlayerEvent::MediaKey(_) => AudioEvent::Raise,
                    };
                    on(e);
                }
            })
            .expect("audio event thread");
        Audio { p }
    }
    pub fn load(&self, src: Src, autoplay: bool) {
        let s = match src {
            Src::File(f) => geonix_player::Source::file(f),
            Src::Url(u) => geonix_player::Source::url(u),
        };
        self.p.load(s, autoplay);
    }
    pub fn preload(&self, src: Option<Src>) {
        self.p.preload(src.map(|s| match s {
            Src::File(f) => geonix_player::Source::file(f),
            Src::Url(u) => geonix_player::Source::url(u),
        }));
    }
    pub fn toggle(&self) {
        self.p.toggle();
    }
    pub fn pause(&self) {
        self.p.pause();
    }
    pub fn seek(&self, secs: f64) {
        self.p.seek(secs);
    }
    pub fn position(&self) -> f64 {
        self.p.position()
    }
    pub fn set_volume(&self, v: f32) {
        self.p.set_volume(v);
    }
    pub fn set_meta(&self, m: Meta) {
        self.p.set_metadata(geonix_player::MediaMeta {
            title: m.title,
            artist: m.artist,
            album: m.album,
            cover_url: Some(m.cover_url),
            duration: m.duration,
        });
    }
}

/// Silent stand-in when built without the player crate.
#[cfg(not(feature = "audio"))]
pub struct Audio {
    on: std::sync::Arc<dyn Fn(AudioEvent) + Send + Sync>,
}

#[cfg(not(feature = "audio"))]
impl Audio {
    pub fn new(_null_sink: bool, on: impl Fn(AudioEvent) + Send + Sync + 'static) -> Audio {
        Audio { on: std::sync::Arc::new(on) }
    }
    pub fn load(&self, _src: Src, _autoplay: bool) {
        (self.on)(AudioEvent::Error("This build has no audio output".into()));
    }
    pub fn preload(&self, _src: Option<Src>) {}
    pub fn toggle(&self) {}
    pub fn pause(&self) {}
    pub fn seek(&self, _secs: f64) {}
    pub fn position(&self) -> f64 {
        0.0
    }
    pub fn set_volume(&self, _v: f32) {}
    pub fn set_meta(&self, _m: Meta) {}
}
