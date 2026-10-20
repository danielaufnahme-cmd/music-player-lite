//! OS media keys / now-playing via souvlaki (feature `media-controls`).

use std::sync::mpsc::Sender;

use crate::{Cmd, MediaMeta, PlaybackState};

#[cfg(feature = "media-controls")]
pub(crate) struct Media {
    c: souvlaki::MediaControls,
}

#[cfg(feature = "media-controls")]
impl Media {
    pub fn new(display_name: &str, dbus_name: &str, tx: Sender<Cmd>) -> Option<Self> {
        use souvlaki::{MediaControlEvent as E, MediaControls, PlatformConfig, SeekDirection};

        let cfg = PlatformConfig { display_name, dbus_name, hwnd: None };
        let mut c = MediaControls::new(cfg).ok()?;
        c.attach(move |e| {
            let dir = |d: SeekDirection| if matches!(d, SeekDirection::Forward) { 1.0 } else { -1.0 };
            let cmd = match e {
                E::Play => Cmd::Play,
                E::Pause => Cmd::Pause,
                E::Toggle => Cmd::Toggle,
                E::Stop => Cmd::Pause,
                E::Next => Cmd::Key(crate::MediaKey::Next),
                E::Previous => Cmd::Key(crate::MediaKey::Previous),
                E::Raise => Cmd::Key(crate::MediaKey::Raise),
                E::Quit => Cmd::Key(crate::MediaKey::Quit),
                E::Seek(d) => Cmd::SeekBy(10.0 * dir(d)),
                E::SeekBy(d, by) => Cmd::SeekBy(by.as_secs_f64() * dir(d)),
                E::SetPosition(p) => Cmd::Seek(p.0.as_secs_f64()),
                E::SetVolume(_) | E::OpenUri(_) => return,
            };
            let _ = tx.send(cmd);
        })
        .ok()?;
        Some(Media { c })
    }

    pub fn set_meta(&mut self, m: &MediaMeta) {
        let _ = self.c.set_metadata(souvlaki::MediaMetadata {
            title: Some(&m.title),
            artist: (!m.artist.is_empty()).then_some(m.artist.as_str()),
            album: (!m.album.is_empty()).then_some(m.album.as_str()),
            cover_url: m.cover_url.as_deref(),
            duration: m.duration.filter(|d| d.is_finite() && *d >= 0.0).map(std::time::Duration::from_secs_f64),
        });
    }

    pub fn set_state(&mut self, st: PlaybackState, pos: f64) {
        use souvlaki::{MediaPlayback, MediaPosition};
        let progress = Some(MediaPosition(std::time::Duration::from_secs_f64(pos.max(0.0))));
        let pb = match st {
            PlaybackState::Playing => MediaPlayback::Playing { progress },
            PlaybackState::Paused | PlaybackState::Loading => MediaPlayback::Paused { progress },
            PlaybackState::Idle | PlaybackState::Stopped => MediaPlayback::Stopped,
        };
        let _ = self.c.set_playback(pb);
    }
}

#[cfg(not(feature = "media-controls"))]
pub(crate) struct Media;

#[cfg(not(feature = "media-controls"))]
impl Media {
    pub fn new(_: &str, _: &str, _: Sender<Cmd>) -> Option<Self> {
        None
    }
    pub fn set_meta(&mut self, _: &MediaMeta) {}
    pub fn set_state(&mut self, _: PlaybackState, _: f64) {}
}
