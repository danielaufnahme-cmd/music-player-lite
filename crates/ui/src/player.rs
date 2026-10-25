//! Queue and playback (port of state/player_controller.dart), lyrics and
//! the cover palette of the playing song.

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::api::{Lyrics, Track};
use crate::app::{fmt_duration, post, track_item, App, Repeat};
use crate::audio::{AudioEvent, Meta, Src};
use crate::LyricLine;

impl App {
    pub fn current(&self) -> Option<&Track> {
        if self.queue.index >= 0 {
            self.queue.tracks.get(self.queue.index as usize)
        } else {
            None
        }
    }

    pub fn up_next(&self) -> &[Track] {
        let start = (self.queue.index + 1).max(0) as usize;
        if start < self.queue.tracks.len() {
            &self.queue.tracks[start..]
        } else {
            &[]
        }
    }

    fn has_next(&self) -> bool {
        (self.queue.index + 1) < self.queue.tracks.len() as i64 || (self.repeat == Repeat::All && !self.queue.tracks.is_empty())
    }

    fn src_for(&self, t: &Track) -> Src {
        match t.file_path.as_ref().filter(|p| !p.is_empty() && std::path::Path::new(p).exists()) {
            Some(p) => Src::File(p.clone()),
            // podcast episode not downloaded: stream the enclosure link
            None if crate::podcasts::episode_ids(&t.video_id).is_some() => Src::Url(t.thumbnail_url.clone()),
            None => Src::Url(self.api.stream_url(&t.video_id)),
        }
    }

    /// Push queue/current-track state into the UI.
    pub fn sync_player_ui(&mut self) {
        let cur = self.current().cloned();
        let p = self.win.global::<crate::Player>();
        p.set_has_track(cur.is_some());
        if let Some(t) = &cur {
            p.set_id(t.video_id.as_str().into());
            p.set_title(t.title.as_str().into());
            p.set_artist(t.artist.as_str().into());
            p.set_album(t.album.as_str().into());
        }
        p.set_shuffle(self.queue.shuffle);
        p.set_repeat(match self.repeat {
            Repeat::Off => 0,
            Repeat::All => 1,
            Repeat::One => 2,
        });
        p.set_has_next(self.has_next());
        let items: Vec<_> = self.up_next().iter().map(track_item).collect();
        self.m_upnext.set_vec(items);
        self.update_preload();
    }

    fn open_current(&mut self) {
        let Some(t) = self.current().cloned() else { return };
        self.pod_before_open();
        self.position = 0.0;
        self.duration = t.duration_sec as f64;
        self.preloaded = None;
        self.audio.load(self.src_for(&t), true);
        self.audio.set_meta(Meta {
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            cover_url: match crate::podcasts::episode_ids(&t.video_id) {
                Some((podcast, _)) => format!("{}/api/podcasts/{podcast}/cover", self.api.base),
                None => self.api.cover_url(&t.video_id),
            },
            duration: if t.duration_sec > 0 { Some(t.duration_sec as f64) } else { None },
        });
        self.pod_on_open(&t);
        self.playing = true;
        self.win.global::<crate::Player>().set_playing(true);
        self.update_position_ui();
        self.sync_player_ui();
        self.track_changed();
        // Count the play (best effort), then refresh the "recent"/"top" rows.
        // Podcast episodes are not songs: they keep their own position instead.
        if self.pod.playing.is_some() {
            return;
        }
        let api = self.api.clone();
        let id = t.video_id.clone();
        self.spawn(async move {
            if api.played(&id).await.is_ok() {
                let (recent, top) = tokio::join!(api.tracks("recent"), api.tracks("plays"));
                post(move |a| {
                    if let Ok(r) = recent {
                        a.recent = Some(r.into_iter().filter(|t| t.last_played.is_some()).take(16).collect());
                    }
                    if let Ok(t) = top {
                        a.top = Some(t.into_iter().filter(|t| t.play_count > 0).take(16).collect());
                    }
                    a.update_home();
                });
            }
        });
    }

    /// Preload the next song for gapless playback.
    pub(crate) fn update_preload(&mut self) {
        if self.repeat == Repeat::One || !self.settings.gapless {
            if self.preloaded.take().is_some() {
                self.audio.preload(None);
            }
            return;
        }
        let next = self.up_next().first().cloned();
        let id = next.as_ref().map(|t| t.video_id.clone());
        if id != self.preloaded {
            self.preloaded = id;
            let src = next.as_ref().map(|t| self.src_for(t));
            self.audio.preload(src);
        }
    }

    pub fn play_list(&mut self, tracks: Vec<Track>, start: usize, shuffle: Option<bool>) {
        if tracks.is_empty() {
            return;
        }
        let want = shuffle.unwrap_or(self.queue.shuffle);
        let start = start.min(tracks.len() - 1);
        let mut queue = tracks.clone();
        let mut index = start as i64;
        if want {
            let first = queue.remove(start);
            shuffle_vec(&mut queue);
            queue.insert(0, first);
            index = 0;
        }
        self.queue.tracks = queue;
        self.queue.index = index;
        self.queue.original = tracks;
        self.queue.shuffle = want;
        self.open_current();
    }

    pub fn track_clicked(&mut self, list: &str, index: usize) {
        let tracks = self.list(list);
        let Some(t) = tracks.get(index) else { return };
        if self.current().map(|c| c.video_id == t.video_id).unwrap_or(false) {
            self.play_pause();
        } else {
            self.play_list(tracks, index, None);
        }
    }

    pub fn play_pause(&mut self) {
        if self.current().is_none() {
            return;
        }
        self.audio.toggle();
    }

    pub fn next(&mut self, auto: bool) {
        if self.queue.tracks.is_empty() {
            return;
        }
        if self.queue.index + 1 < self.queue.tracks.len() as i64 {
            self.queue.index += 1;
        } else if self.repeat == Repeat::All {
            self.queue.index = 0;
        } else {
            if auto && self.pod_current_ids().is_none() && self.continue_with_recs() {
                return;
            }
            if auto {
                self.playing = false;
                self.win.global::<crate::Player>().set_playing(false);
                self.audio.pause();
                self.audio.seek(0.0);
                self.position = 0.0;
                self.update_position_ui();
            }
            return;
        }
        self.open_current();
    }

    pub fn previous(&mut self) {
        if self.queue.tracks.is_empty() {
            return;
        }
        // Restart the song unless we're at its very start.
        if self.audio.position() > 3.0 || self.position > 3.0 || self.queue.index == 0 {
            self.audio.seek(0.0);
            self.position = 0.0;
            self.update_position_ui();
            return;
        }
        self.queue.index -= 1;
        self.open_current();
    }

    pub fn seek_fraction(&mut self, f: f32) {
        if self.duration > 0.0 {
            let s = self.duration * f as f64;
            self.audio.seek(s);
            self.position = s;
            self.update_position_ui();
        }
    }

    pub fn seek_ms(&mut self, ms: i64) {
        let s = ms as f64 / 1000.0;
        self.audio.seek(s);
        self.position = s;
        self.update_position_ui();
    }

    pub fn set_volume(&mut self, v: f32) {
        let v = v.clamp(0.0, 100.0);
        self.volume = v / 100.0;
        self.audio.set_volume(self.volume);
        self.win.global::<crate::Player>().set_volume(v);
        self.settings.volume = self.volume;
    }

    pub fn toggle_shuffle(&mut self) {
        let current = self.current().cloned();
        if !self.queue.shuffle {
            let mut upcoming = self.up_next().to_vec();
            shuffle_vec(&mut upcoming);
            let played: Vec<Track> =
                if self.queue.index >= 0 { self.queue.tracks[..=(self.queue.index as usize)].to_vec() } else { vec![] };
            self.queue.original = self.queue.tracks.clone();
            self.queue.tracks = played.into_iter().chain(upcoming).collect();
            self.queue.shuffle = true;
        } else {
            let original = if self.queue.original.is_empty() { self.queue.tracks.clone() } else { self.queue.original.clone() };
            let idx = current.and_then(|c| original.iter().position(|t| t.video_id == c.video_id));
            if let Some(i) = idx {
                self.queue.index = i as i64;
            }
            self.queue.tracks = original;
            self.queue.shuffle = false;
        }
        self.sync_player_ui();
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
        self.sync_player_ui();
    }

    pub fn play_next(&mut self, t: Track) {
        if self.current().is_none() {
            self.play_list(vec![t], 0, None);
            return;
        }
        let at = (self.queue.index + 1) as usize;
        self.queue.tracks.insert(at, t.clone());
        self.queue.original.push(t.clone());
        self.sync_player_ui();
        self.snack(format!("“{}” plays next", t.title));
    }

    pub fn add_to_queue(&mut self, t: Track) {
        if self.current().is_none() {
            self.play_list(vec![t], 0, None);
            return;
        }
        self.queue.tracks.push(t.clone());
        self.queue.original.push(t.clone());
        self.sync_player_ui();
        self.snack(format!("Added “{}” to the queue", t.title));
    }

    pub fn move_up_next(&mut self, from: i64, to: i64) {
        let base = self.queue.index + 1;
        let n = self.up_next().len() as i64;
        if n == 0 {
            return;
        }
        let to = to.clamp(0, n - 1);
        if from != to && from >= 0 && from < n {
            let item = self.queue.tracks.remove((base + from) as usize);
            self.queue.tracks.insert((base + to) as usize, item);
        }
        self.sync_player_ui();
    }

    pub fn remove_up_next(&mut self, i: i64) {
        let at = self.queue.index + 1 + i;
        if at >= 0 && (at as usize) < self.queue.tracks.len() {
            self.queue.tracks.remove(at as usize);
        }
        self.sync_player_ui();
    }

    pub fn clear_up_next(&mut self) {
        if self.queue.index < 0 {
            return;
        }
        self.queue.tracks.truncate((self.queue.index + 1) as usize);
        self.sync_player_ui();
    }

    pub fn jump_to(&mut self, i: i64) {
        let at = self.queue.index + 1 + i;
        if at >= 0 && (at as usize) < self.queue.tracks.len() {
            self.queue.index = at;
            self.open_current();
        }
    }

    pub fn on_audio(&mut self, e: AudioEvent) {
        match e {
            AudioEvent::Loaded(d) => {
                if let Some(d) = d.filter(|d| *d > 0.0) {
                    self.duration = d;
                }
                self.update_position_ui();
                self.pod_loaded();
            }
            AudioEvent::Position(p, d) => {
                self.position = p;
                if let Some(d) = d.filter(|d| *d > 0.0) {
                    self.duration = d;
                }
                self.update_position_ui();
                self.pod_position(p);
            }
            AudioEvent::Playing(p) => {
                if self.playing != p {
                    self.playing = p;
                    self.win.global::<crate::Player>().set_playing(p);
                    if !p {
                        self.pod_paused();
                    }
                    self.start_waves();
                }
            }
            AudioEvent::Advanced(d) => {
                // The preloaded song started gaplessly.
                self.pod_ended();
                self.queue.index += 1;
                self.preloaded = None;
                self.position = 0.0;
                if let Some(d) = d {
                    self.duration = d;
                }
                self.sync_player_ui();
                self.track_changed();
                self.update_position_ui();
                if let Some(t) = self.current().cloned() {
                    self.pod_on_open(&t);
                }
            }
            AudioEvent::Ended => {
                self.pod_ended();
                if self.repeat == Repeat::One {
                    self.open_current();
                } else {
                    self.next(true);
                }
            }
            AudioEvent::Error(e) => self.snack(format!("Playback problem: {e}")),
            AudioEvent::Next => self.next(false),
            AudioEvent::Previous => self.previous(),
            AudioEvent::Raise => {
                let _ = self.win.window().show();
            }
        }
    }

    pub fn update_position_ui(&mut self) {
        let p = self.win.global::<crate::Player>();
        p.set_position(self.position as f32);
        p.set_duration(self.duration as f32);
        p.set_position_text(fmt_duration(self.position).into());
        p.set_duration_text(fmt_duration(self.duration).into());
        if let Some(l) = &self.current_lyrics {
            let line = l.line_at((self.position * 1000.0) as i64);
            if p.get_lyric_line() != line {
                p.set_lyric_line(line);
            }
        }
    }

    /// The playing song changed: lyrics and cover colours.
    pub(crate) fn track_changed(&mut self) {
        let Some(t) = self.current().cloned() else { return };
        let id = t.video_id.clone();
        // lyrics (episodes have none)
        let episode = self.pod_current_ids().is_some();
        match self.lyrics.get(&id).cloned().or_else(|| episode.then(Lyrics::default)) {
            Some(l) => self.show_lyrics(Some(l)),
            None if !self.settings.show_lyrics => self.show_lyrics(None),
            None => {
                self.current_lyrics = None;
                self.win.global::<crate::Player>().set_lyrics_state(0);
                self.win.global::<crate::Player>().set_lyric_line(-1);
                let api = self.api.clone();
                let id2 = id.clone();
                self.spawn(async move {
                    let l = api.lyrics(&id2).await.unwrap_or_default();
                    post(move |a| {
                        if a.lyrics.len() > 32 {
                            a.lyrics.clear();
                        }
                        a.lyrics.insert(id2.clone(), l.clone());
                        if a.current().map(|c| c.video_id == id2).unwrap_or(false) {
                            a.show_lyrics(Some(l));
                        }
                    });
                });
            }
        }
        // colours
        match self.palettes.get(&id).copied() {
            Some(p) => {
                self.cover_palette = Some(p);
                self.apply_theme();
            }
            None => {
                let api = self.api.clone();
                self.spawn(async move {
                    let p = crate::covers::palette(&api, &id).await;
                    post(move |a| {
                        if a.palettes.len() > 64 {
                            a.palettes.clear();
                        }
                        a.palettes.insert(id.clone(), p);
                        if a.current().map(|c| c.video_id == id).unwrap_or(false) {
                            a.cover_palette = Some(p);
                            a.apply_theme();
                        }
                    });
                });
            }
        }
    }

    pub(crate) fn show_lyrics(&mut self, l: Option<Lyrics>) {
        let p = self.win.global::<crate::Player>();
        match &l {
            Some(l) if !l.lines.is_empty() => {
                // Karaoke off: synced lyrics show as plain text, no moving highlight.
                p.set_lyrics_state(if l.synced && self.settings.karaoke { 3 } else { 2 });
                p.set_lyrics(ModelRc::new(VecModel::from(
                    l.lines
                        .iter()
                        .map(|(t, s)| LyricLine { text: s.as_str().into(), time_ms: t.map(|t| t as i32).unwrap_or(-1) })
                        .collect::<Vec<_>>(),
                )));
                p.set_lyrics_source(l.source.clone().unwrap_or_default().into());
                p.set_lyric_line(l.line_at((self.position * 1000.0) as i64));
            }
            _ => {
                p.set_lyrics_state(1);
                p.set_lyrics(ModelRc::default());
                p.set_lyric_line(-1);
            }
        }
        self.current_lyrics = l;
    }

    pub fn add_current_to_playlist(&mut self) {
        if self.pod_current_ids().is_some() {
            self.snack("Podcast episodes can’t be added to playlists");
            return;
        }
        if let Some(t) = self.current().cloned() {
            self.add_to_playlist(t);
        }
    }
}

/// Fisher–Yates with a small xorshift seeded from the clock.
fn shuffle_vec<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x9E37_79B9) | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}
