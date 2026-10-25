//! Podcasts page and its controller: search, subscriptions, the episode list
//! of one podcast, downloads and the listening position of episodes.
//!
//! Episodes play through the normal queue as tracks with the id
//! `podcast:<podcast>:<episode>`. Nothing runs while the page is closed
//! except the position saves of a playing episode (every ~10 s, on pause,
//! on switching away and at the end).

use std::rc::Rc;
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::api::Track;
use crate::app::{fmt_duration, plural, post, App, Route};
use crate::podcast_api::{Episode, Hit, PodApi, PodEvent, Podcast};
use crate::{CardItem, EpisodeItem, Pod, PodcastItem};

/// Position is saved to the server this often while an episode plays.
const SAVE_EVERY: f64 = 10.0;
/// Earlier than this into an episode there is nothing worth resuming.
const RESUME_MIN: f64 = 5.0;

pub struct PodState {
    pub api: PodApi,
    pub subs: Vec<Podcast>,
    pub hits: Vec<Hit>,
    pub search_seq: u64,
    pub debounce: slint::Timer,
    pub current: Option<Podcast>,
    pub episodes: Vec<Episode>,
    pub ep_seq: u64,
    pub events: Option<tokio::task::JoinHandle<()>>,
    /// (podcast, episode) that is loaded in the player.
    pub playing: Option<(i64, i64)>,
    /// Position of the last save, to space the saves out.
    pub last_saved: f64,
    /// Seek here once the loaded episode reports it is ready.
    pub resume_to: Option<f64>,
    pub m_subs: Rc<VecModel<CardItem>>,
    pub m_hits: Rc<VecModel<PodcastItem>>,
    pub m_eps: Rc<VecModel<EpisodeItem>>,
}

impl PodState {
    pub fn new(base: String) -> Self {
        Self {
            api: PodApi::new(base),
            subs: Vec::new(),
            hits: Vec::new(),
            search_seq: 0,
            debounce: slint::Timer::default(),
            current: None,
            episodes: Vec::new(),
            ep_seq: 0,
            events: None,
            playing: None,
            last_saved: 0.0,
            resume_to: None,
            m_subs: Rc::new(VecModel::default()),
            m_hits: Rc::new(VecModel::default()),
            m_eps: Rc::new(VecModel::default()),
        }
    }
}

/// `podcast:<podcast>:<episode>` to its two ids.
pub fn episode_ids(video_id: &str) -> Option<(i64, i64)> {
    let mut it = video_id.strip_prefix("podcast:")?.split(':');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn fmt_date(ms: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, m, d) = civil_from_days(ms / 86_400_000);
    format!("{} {d}, {y}", MONTHS[(m - 1) as usize % 12])
}

fn is_link(q: &str) -> bool {
    let q = q.trim();
    (q.starts_with("http://") || q.starts_with("https://")) && !q.contains(char::is_whitespace)
}

pub fn episode_item(e: &Episode) -> EpisodeItem {
    let mut parts = Vec::new();
    if e.published_at > 0 {
        parts.push(fmt_date(e.published_at));
    }
    if e.duration_sec > 0 {
        parts.push(fmt_duration(e.duration_sec as f64));
    }
    let resume = if e.played {
        "Played".to_string()
    } else if e.position_sec >= RESUME_MIN {
        format!("Resume at {}", fmt_duration(e.position_sec))
    } else {
        String::new()
    };
    EpisodeItem {
        id: e.id as i32,
        key: format!("podcast:{}:{}", e.podcast_id, e.id).into(),
        title: if e.title.is_empty() { "Untitled episode".into() } else { e.title.as_str().into() },
        subtitle: parts.join(" · ").into(),
        resume: resume.into(),
        state: match e.status.as_str() {
            "done" => 1,
            "queued" => 2,
            "downloading" => 3,
            "failed" => 4,
            _ => 0,
        },
        progress: (e.progress / 100.0).clamp(0.0, 1.0) as f32,
        listened: if !e.played && e.duration_sec > 0 { (e.position_sec / e.duration_sec as f64).clamp(0.0, 1.0) as f32 } else { 0.0 },
        played: e.played,
    }
}

fn hit_item(h: &Hit) -> PodcastItem {
    PodcastItem {
        id: 0,
        title: h.title.as_str().into(),
        author: h.author.as_str().into(),
        cover: h.image_url.as_str().into(),
        meta: SharedString::new(),
        feed_url: h.feed_url.as_str().into(),
        subscribed: h.subscribed,
    }
}

fn card(p: &Podcast) -> CardItem {
    CardItem {
        kind: 3,
        id: format!("podcast:{}", p.id).into(),
        title: p.title.as_str().into(),
        subtitle: p.author.as_str().into(),
        downloadable: false,
        pid: p.id as i32,
        covers: ModelRc::default(),
        discover: false,
    }
}

impl App {
    // ------------------------------------------------------------ page life

    /// A podcast page was opened: load it and listen for download progress.
    pub fn pod_enter(&mut self) {
        if self.pod.events.is_none() {
            let api = self.pod.api.clone();
            self.pod.events = Some(self.rt.spawn(async move {
                loop {
                    let _ = api
                        .events(|e| {
                            post(move |a| match e {
                                PodEvent::Episode(ep) => a.pod_apply_episode(ep),
                                PodEvent::Resync => {
                                    if let Route::Podcast(id) = a.route() {
                                        a.pod_load_episodes(id, false);
                                    }
                                }
                            })
                        })
                        .await;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }));
        }
        match self.route() {
            Route::Podcast(id) => self.pod_load_episodes(id, true),
            _ => self.pod_load_subs(),
        }
    }

    /// Left the podcast pages: stop listening.
    pub fn pod_leave(&mut self) {
        if let Some(h) = self.pod.events.take() {
            h.abort();
        }
        self.pod.debounce.stop();
    }

    // ------------------------------------------------------------ subscribed list

    pub fn pod_load_subs(&mut self) {
        let ui = self.win.global::<Pod>();
        if self.pod.subs.is_empty() {
            ui.set_subs_state(0);
        }
        let api = self.pod.api.clone();
        self.spawn(async move {
            let r = api.subscribed().await;
            post(move |a| a.pod_set_subs(r));
        });
    }

    fn pod_set_subs(&mut self, r: Result<Vec<Podcast>, String>) {
        let ui = self.win.global::<Pod>();
        match r {
            Ok(list) => {
                self.pod.subs = list;
                self.pod.m_subs.set_vec(self.pod.subs.iter().map(card).collect::<Vec<_>>());
                ui.set_subs_state(1);
            }
            Err(e) => {
                ui.set_subs_error(e.into());
                ui.set_subs_state(2);
            }
        }
    }

    pub fn pod_retry(&mut self) {
        match self.route() {
            Route::Podcast(id) => self.pod_load_episodes(id, true),
            _ => self.pod_load_subs(),
        }
    }

    pub fn pod_open_sub(&mut self, i: usize) {
        if let Some(p) = self.pod.subs.get(i) {
            let id = p.id;
            self.push(Route::Podcast(id));
        }
    }

    /// Open the podcast with the most episodes (screenshot tour).
    pub fn pod_open_biggest(&mut self) {
        if let Some(i) = (0..self.pod.subs.len()).max_by_key(|i| self.pod.subs[*i].episode_count) {
            self.pod_open_sub(i);
        }
    }

    // ------------------------------------------------------------ search

    pub fn pod_set_query(&mut self, q: String) {
        let ui = self.win.global::<Pod>();
        ui.set_query(q.as_str().into());
        self.pod.search_seq += 1;
        self.pod.debounce.stop();
        let q = q.trim().to_string();
        if q.is_empty() {
            ui.set_search_state(0);
            self.pod.hits.clear();
            self.pod.m_hits.set_vec(vec![]);
        } else if is_link(&q) {
            ui.set_search_state(5);
        } else {
            // Search a moment after the last key (the search service limits requests).
            ui.set_search_state(1);
            self.pod.debounce.start(slint::TimerMode::SingleShot, Duration::from_millis(600), || crate::app::with_app(|a| a.pod_search()));
        }
    }

    /// Enter in the search box.
    pub fn pod_submit(&mut self) {
        let q = self.win.global::<Pod>().get_query().trim().to_string();
        if is_link(&q) {
            self.pod_subscribe(q, None);
        } else if !q.is_empty() {
            self.pod.debounce.stop();
            self.pod_search();
        }
    }

    fn pod_search(&mut self) {
        let ui = self.win.global::<Pod>();
        let q = ui.get_query().trim().to_string();
        if q.is_empty() || is_link(&q) {
            return;
        }
        ui.set_search_state(1);
        self.pod.search_seq += 1;
        let seq = self.pod.search_seq;
        let api = self.pod.api.clone();
        self.spawn(async move {
            let r = api.search(&q).await;
            post(move |a| {
                if seq != a.pod.search_seq {
                    return;
                }
                let ui = a.win.global::<Pod>();
                match r {
                    Ok(hits) => {
                        a.pod.m_hits.set_vec(hits.iter().map(hit_item).collect::<Vec<_>>());
                        ui.set_search_state(if hits.is_empty() { 3 } else { 2 });
                        a.pod.hits = hits;
                    }
                    Err(e) => {
                        ui.set_search_error(e.into());
                        ui.set_search_state(4);
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------ subscribing

    pub fn pod_open_hit(&mut self, i: usize) {
        let Some(h) = self.pod.hits.get(i).cloned() else { return };
        match self.pod.subs.iter().find(|p| p.feed_url == h.feed_url).map(|p| p.id) {
            Some(id) => self.push(Route::Podcast(id)),
            None => self.pod_subscribe(h.feed_url, Some(h.image_url)),
        }
    }

    pub fn pod_subscribe_hit(&mut self, i: usize) {
        let Some(h) = self.pod.hits.get(i).cloned() else { return };
        if let Some(id) = self.pod.subs.iter().find(|p| p.feed_url == h.feed_url).map(|p| p.id) {
            self.push(Route::Podcast(id));
        } else {
            self.pod_subscribe(h.feed_url, Some(h.image_url));
        }
    }

    /// Subscribe to a feed and open its page.
    pub fn pod_subscribe(&mut self, feed_url: String, image_url: Option<String>) {
        let ui = self.win.global::<Pod>();
        if ui.get_busy() {
            return;
        }
        ui.set_busy(true);
        let api = self.pod.api.clone();
        self.spawn(async move {
            let r = api.subscribe(&feed_url, image_url.as_deref().filter(|u| !u.is_empty())).await;
            post(move |a| {
                a.win.global::<Pod>().set_busy(false);
                match r {
                    Ok(p) => {
                        a.snack(format!("Subscribed to “{}”", p.title));
                        let id = p.id;
                        a.pod.subs.retain(|s| s.id != id);
                        a.pod.subs.push(p);
                        a.pod.subs.sort_by_key(|s| s.title.to_lowercase());
                        a.pod.m_subs.set_vec(a.pod.subs.iter().map(card).collect::<Vec<_>>());
                        // search results show it as subscribed
                        let ui = a.win.global::<Pod>();
                        ui.set_search_state(0);
                        ui.set_query(SharedString::new());
                        ui.set_subs_state(1);
                        a.pod.hits.clear();
                        a.pod.m_hits.set_vec(vec![]);
                        a.push(Route::Podcast(id));
                    }
                    Err(e) => a.snack(format!("Couldn’t subscribe: {e}")),
                }
            });
        });
    }

    pub fn pod_unsubscribe(&mut self) {
        let Some(p) = self.pod.current.clone() else { return };
        let api = self.pod.api.clone();
        self.spawn(async move {
            let r = api.unsubscribe(p.id).await;
            post(move |a| match r {
                Ok(()) => {
                    a.snack(format!("Unsubscribed from “{}”. Downloaded episodes stay on disk.", p.title));
                    a.pod.subs.retain(|s| s.id != p.id);
                    a.pod.m_subs.set_vec(a.pod.subs.iter().map(card).collect::<Vec<_>>());
                    a.pop();
                }
                Err(e) => a.snack(format!("Couldn’t unsubscribe: {e}")),
            });
        });
    }

    // ------------------------------------------------------------ one podcast

    pub fn pod_load_episodes(&mut self, id: i64, show_loading: bool) {
        self.pod.ep_seq += 1;
        let seq = self.pod.ep_seq;
        let ui = self.win.global::<Pod>();
        if show_loading {
            ui.set_ep_state(0);
            self.pod.episodes.clear();
            self.pod.m_eps.set_vec(vec![]);
            self.pod.current = self.pod.subs.iter().find(|p| p.id == id).cloned();
            self.pod_update_header();
        }
        let api = self.pod.api.clone();
        self.spawn(async move {
            let (subs, eps) = tokio::join!(api.subscribed(), api.episodes(id));
            post(move |a| {
                if seq != a.pod.ep_seq {
                    return;
                }
                if let Ok(s) = subs {
                    a.pod.subs = s;
                    a.pod.m_subs.set_vec(a.pod.subs.iter().map(card).collect::<Vec<_>>());
                    a.win.global::<Pod>().set_subs_state(1);
                }
                a.pod.current = a.pod.subs.iter().find(|p| p.id == id).cloned().or(a.pod.current.take());
                let ui = a.win.global::<Pod>();
                match eps {
                    Ok(list) => {
                        a.pod.m_eps.set_vec(list.iter().map(episode_item).collect::<Vec<_>>());
                        a.pod.episodes = list;
                        ui.set_ep_state(1);
                    }
                    Err(e) => {
                        ui.set_ep_error(e.into());
                        ui.set_ep_state(2);
                    }
                }
                a.pod_update_header();
            });
        });
    }

    fn pod_update_header(&mut self) {
        let ui = self.win.global::<Pod>();
        let Some(p) = &self.pod.current else { return };
        let total = self.pod.episodes.len() as i64;
        let unplayed = self.pod.episodes.iter().filter(|e| !e.played).count() as i64;
        let meta = if total == 0 {
            String::new()
        } else if unplayed > 0 {
            format!("{} · {unplayed} unplayed", plural(total, "episode"))
        } else {
            plural(total, "episode")
        };
        ui.set_current(PodcastItem {
            id: p.id as i32,
            title: p.title.as_str().into(),
            author: p.author.as_str().into(),
            cover: format!("podcast:{}", p.id).into(),
            meta: meta.into(),
            feed_url: p.feed_url.as_str().into(),
            subscribed: p.subscribed,
        });
        ui.set_current_error(p.last_error.clone().unwrap_or_default().into());
    }

    pub fn pod_refresh(&mut self) {
        let Some(p) = self.pod.current.clone() else { return };
        let ui = self.win.global::<Pod>();
        if ui.get_refreshing() {
            return;
        }
        ui.set_refreshing(true);
        let api = self.pod.api.clone();
        self.spawn(async move {
            let r = api.refresh(p.id).await;
            post(move |a| {
                a.win.global::<Pod>().set_refreshing(false);
                match r {
                    Ok(0) => a.snack("No new episodes"),
                    Ok(n) => a.snack(format!("{}", plural(n, "new episode"))),
                    Err(e) => a.snack(format!("Couldn’t check the feed: {e}")),
                }
                if a.route() == Route::Podcast(p.id) {
                    a.pod_load_episodes(p.id, false);
                }
            });
        });
    }

    // ------------------------------------------------------------ episodes

    /// An episode changed on the server (download progress, finished, played).
    pub fn pod_apply_episode(&mut self, ep: Episode) {
        let Some(i) = self.pod.episodes.iter().position(|e| e.id == ep.id) else { return };
        self.pod.m_eps.set_row_data(i, episode_item(&ep));
        self.pod.episodes[i] = ep;
        self.pod_update_header();
    }

    pub fn pod_download(&mut self, i: usize) {
        let Some(e) = self.pod.episodes.get(i) else { return };
        let (id, api) = (e.id, self.pod.api.clone());
        self.spawn(async move {
            match api.download(id).await {
                Ok(ep) => post(move |a| a.pod_apply_episode(ep)),
                Err(e) => post(move |a| a.snack(format!("Couldn’t download: {e}"))),
            }
        });
    }

    pub fn pod_delete_file(&mut self, i: usize) {
        let Some(e) = self.pod.episodes.get(i) else { return };
        let (id, api) = (e.id, self.pod.api.clone());
        self.spawn(async move {
            match api.delete_file(id).await {
                Ok(ep) => post(move |a| {
                    a.pod_apply_episode(ep);
                    a.snack("Deleted the downloaded file");
                }),
                Err(e) => post(move |a| a.snack(format!("Couldn’t delete: {e}"))),
            }
        });
    }

    /// The episode as a player track. A downloaded file plays from disk; the
    /// enclosure link (kept in `thumbnail_url`, unused for episodes) streams.
    fn episode_track(&self, e: &Episode) -> Track {
        let done = e.status == "done" && e.file_path.is_some();
        Track {
            video_id: format!("podcast:{}:{}", e.podcast_id, e.id),
            title: e.title.clone(),
            artist: self.pod.current.as_ref().map(|p| p.title.clone()).unwrap_or_default(),
            duration_sec: e.duration_sec,
            thumbnail_url: e.enclosure_url.clone(),
            file_path: if done { e.file_path.clone() } else { None },
            downloaded: done,
            ..Default::default()
        }
    }

    pub fn pod_play(&mut self, i: usize) {
        let Some(e) = self.pod.episodes.get(i).cloned() else { return };
        let t = self.episode_track(&e);
        if self.current().is_some_and(|c| c.video_id == t.video_id) {
            self.play_pause();
        } else {
            self.play_list(vec![t], 0, None);
        }
    }

    // ------------------------------------------------------------ position

    /// Is the loaded track a podcast episode?
    pub fn pod_current_ids(&self) -> Option<(i64, i64)> {
        self.current().and_then(|t| episode_ids(&t.video_id))
    }

    /// Before another track is opened: store where the old episode stopped.
    pub fn pod_before_open(&mut self) {
        if let Some((_, e)) = self.pod.playing.take() {
            self.pod_save(e, self.position, self.duration);
        }
    }

    /// The new track is loading: remember it and pick the saved position.
    pub fn pod_on_open(&mut self, t: &Track) {
        self.pod.resume_to = None;
        self.pod.playing = episode_ids(&t.video_id);
        self.pod.last_saved = 0.0;
        let Some((_, e)) = self.pod.playing else { return };
        if let Some(ep) = self.pod.episodes.iter().find(|x| x.id == e).filter(|x| !x.played && x.position_sec >= RESUME_MIN) {
            self.pod.resume_to = Some(ep.position_sec);
            self.pod.last_saved = ep.position_sec;
        }
    }

    /// The player is ready: jump to the saved position.
    pub fn pod_loaded(&mut self) {
        if let Some(s) = self.pod.resume_to.take() {
            self.audio.seek(s);
            self.position = s;
            self.update_position_ui();
        }
    }

    /// Every position tick of the player; saves every [`SAVE_EVERY`] seconds.
    pub fn pod_position(&mut self, pos: f64) {
        let Some((_, e)) = self.pod.playing else { return };
        if self.pod.resume_to.is_some() || (pos - self.pod.last_saved).abs() < SAVE_EVERY {
            return;
        }
        self.pod.last_saved = pos;
        self.pod_save(e, pos, self.duration);
    }

    /// Paused: save right away.
    pub fn pod_paused(&mut self) {
        if let Some((_, e)) = self.pod.playing {
            if self.pod.resume_to.is_none() {
                self.pod.last_saved = self.position;
                self.pod_save(e, self.position, self.duration);
            }
        }
    }

    /// The episode played to its end: save it as finished.
    pub fn pod_ended(&mut self) {
        if let Some((_, e)) = self.pod.playing.take() {
            let end = if self.duration > 0.0 { self.duration } else { self.position };
            self.pod_save(e, end, self.duration);
        }
    }

    fn pod_save(&mut self, episode: i64, position: f64, duration: f64) {
        let api = self.pod.api.clone();
        self.spawn(async move {
            if let Ok(ep) = api.save_position(episode, position, duration).await {
                post(move |a| a.pod_apply_episode(ep));
            }
        });
    }

    /// At quit: write the position of a playing episode before the runtime stops.
    pub fn pod_save_blocking(&mut self) {
        if let Some((_, e)) = self.pod.playing.take() {
            let (api, pos, dur) = (self.pod.api.clone(), self.position, self.duration);
            let _ = self.rt.block_on(async move { tokio::time::timeout(Duration::from_secs(2), api.save_position(e, pos, dur)).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_ids_round_trip() {
        assert_eq!(episode_ids("podcast:3:77"), Some((3, 77)));
        assert_eq!(episode_ids("podcast:3"), None);
        assert_eq!(episode_ids("dQw4w9WgXcQ"), None);
        assert_eq!(episode_ids("podcast:x:1"), None);
    }

    #[test]
    fn links_and_dates() {
        assert!(is_link(" https://example.com/feed.xml "));
        assert!(!is_link("https://example.com/a b"));
        assert!(!is_link("daft punk"));
        assert_eq!(fmt_date(1_749_556_800_000), "Jun 10, 2025");
        assert_eq!(fmt_date(0), "Jan 1, 1970");
    }

    #[test]
    fn episode_rows_show_state_resume_and_played() {
        let mut e = Episode {
            id: 7,
            podcast_id: 2,
            title: "Hello".into(),
            published_at: 1_749_556_800_000,
            duration_sec: 3723,
            status: "downloading".into(),
            progress: 42.0,
            position_sec: 600.0,
            ..Default::default()
        };
        let it = episode_item(&e);
        assert_eq!(it.key, "podcast:2:7");
        assert_eq!(it.subtitle, "Jun 10, 2025 · 1:02:03");
        assert_eq!(it.resume, "Resume at 10:00");
        assert_eq!(it.state, 3);
        assert!((it.progress - 0.42).abs() < 1e-6);
        assert!(it.listened > 0.15 && it.listened < 0.17);
        e.played = true;
        e.status = "done".into();
        let it = episode_item(&e);
        assert_eq!((it.resume.as_str(), it.state, it.listened), ("Played", 1, 0.0));
        e.played = false;
        e.position_sec = 2.0;
        assert_eq!(episode_item(&e).resume, "");
    }
}
