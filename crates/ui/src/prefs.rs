//! The Settings page: builds the rows, applies changes live, persists UI
//! settings to ui.json (crate::settings) and server settings through
//! `GET/PATCH /api/settings`, and exports / imports both as one JSON file.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{json, Map, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{post, with_app, App};
use crate::settings::{Settings, ThemeSource, COVER_CACHE_CHOICES, FPS_CHOICES};
use crate::{PrefRow, Prefs};

/// Bump when the export format changes incompatibly.
pub const EXPORT_VERSION: u64 = 1;
pub const EXPORT_APP: &str = "geonix-lite";

const KBPS: [u64; 4] = [0, 320, 192, 128];
const KBPS_LABELS: [&str; 4] = ["Best", "320 kbps", "192 kbps", "128 kbps"];

/// Server defaults (mirror of geonix_server::settings::ServerSettings::default()).
pub fn server_defaults() -> Value {
    json!({
        "download_dir": "",
        "audio_kbps": 0,
        "parallel_downloads": 3,
        "auto_retry": true,
        "device_sync": true,
        "auto_sync_on_open": true,
        "phone_page": true,
        "daily_mix": true,
        "discover_mix": true,
        "private_listening": false,
    })
}

#[derive(Default)]
struct PageState {
    /// Last server settings we know, None until loaded.
    server: Option<Value>,
    server_error: Option<String>,
    /// Text being typed into fields (not applied yet).
    dir_text: Option<String>,
    file_text: Option<String>,
    /// Live value while the volume slider is dragged.
    volume_drag: Option<f32>,
    model: Option<Rc<VecModel<PrefRow>>>,
}

thread_local! {
    static PAGE: RefCell<PageState> = RefCell::new(PageState::default());
}

fn default_file() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("geonix-lite-settings.json")
}

fn expand(p: &str) -> PathBuf {
    let p = p.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

// ------------------------------------------------------------ export / import (pure)

/// The export file: `{"app", "version", "ui", "server"}`.
pub fn build_export(ui: &Settings, server: Option<&Value>) -> String {
    let v = json!({
        "app": EXPORT_APP,
        "version": EXPORT_VERSION,
        "ui": ui,
        "server": server.cloned().unwrap_or(Value::Null),
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

/// What a valid settings file contains.
#[derive(Debug)]
pub struct Imported {
    pub ui: Option<Settings>,
    pub server: Option<Map<String, Value>>,
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "true/false",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// Every key must exist in `defaults` and have the same JSON type.
fn check_keys(section: &str, given: &Map<String, Value>, defaults: &Value) -> Result<(), String> {
    for (k, v) in given {
        let Some(d) = defaults.get(k) else {
            return Err(format!("unknown setting \"{section}.{k}\""));
        };
        let same = matches!(
            (d, v),
            (Value::Bool(_), Value::Bool(_)) | (Value::Number(_), Value::Number(_)) | (Value::String(_), Value::String(_))
        );
        if !same {
            return Err(format!("\"{section}.{k}\" should be {}, not {}", json_kind(d), json_kind(v)));
        }
    }
    Ok(())
}

/// Validate a settings file. Nothing is applied here.
pub fn parse_export(text: &str) -> Result<Imported, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not a valid JSON file ({e})"))?;
    let Value::Object(top) = v else {
        return Err("not a Music Player Lite settings file".into());
    };
    if let Some(app) = top.get("app") {
        if app.as_str() != Some(EXPORT_APP) {
            return Err("this file is not from Music Player Lite".into());
        }
    }
    let version = match top.get("version") {
        None => return Err("not a Music Player Lite settings file (no version)".into()),
        Some(v) => v.as_u64().ok_or("the version field must be a whole number")?,
    };
    if version == 0 || version > EXPORT_VERSION {
        return Err(format!("settings file version {version} is not supported (this app reads version {EXPORT_VERSION})"));
    }
    for k in top.keys() {
        if !matches!(k.as_str(), "app" | "version" | "ui" | "server") {
            return Err(format!("unknown section \"{k}\""));
        }
    }
    let ui = match top.get("ui") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => {
            let defaults = serde_json::to_value(Settings::default()).unwrap_or_default();
            check_keys("ui", m, &defaults)?;
            let s: Settings = serde_json::from_value(Value::Object(m.clone())).map_err(|e| format!("ui settings: {e}"))?;
            if !FPS_CHOICES.contains(&s.max_fps) {
                return Err(format!("\"ui.max_fps\" must be one of {FPS_CHOICES:?}"));
            }
            if !(0.0..=1.0).contains(&s.volume) || !(0.0..=1.0).contains(&s.default_volume) {
                return Err("volumes must be between 0 and 1".into());
            }
            Some(s.sanitized())
        }
        Some(_) => return Err("\"ui\" must be an object".into()),
    };
    let server = match top.get("server") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => {
            check_keys("server", m, &server_defaults())?;
            if let Some(q) = m.get("audio_kbps").and_then(Value::as_u64) {
                if !KBPS.contains(&q) {
                    return Err("\"server.audio_kbps\" must be 0, 128, 192 or 320".into());
                }
            }
            if let Some(n) = m.get("parallel_downloads").and_then(Value::as_u64) {
                if !(1..=4).contains(&n) {
                    return Err("\"server.parallel_downloads\" must be between 1 and 4".into());
                }
            }
            Some(m.clone())
        }
        Some(_) => return Err("\"server\" must be an object".into()),
    };
    if ui.is_none() && server.is_none() {
        return Err("the file contains no settings".into());
    }
    Ok(Imported { ui, server })
}

// ------------------------------------------------------------ rows

struct Rows(Vec<PrefRow>);

impl Rows {
    fn base(kind: i32, key: &str, label: &str, detail: &str) -> PrefRow {
        PrefRow {
            kind,
            key: key.into(),
            label: label.into(),
            detail: detail.into(),
            on: false,
            options: ModelRc::default(),
            selected: 0,
            value: 0.0,
            text: SharedString::default(),
            note: SharedString::default(),
            enabled: true,
            first: false,
            last: false,
            browse: false,
        }
    }
    fn group(&mut self, label: &str, detail: &str) {
        if let Some(prev) = self.0.last_mut() {
            prev.last = true;
        }
        let mut r = Self::base(0, "", label, detail);
        r.first = self.0.is_empty();
        self.0.push(r);
    }
    fn push(&mut self, mut r: PrefRow) -> &mut PrefRow {
        r.first = self.0.last().map(|p| p.kind == 0).unwrap_or(true);
        self.0.push(r);
        self.0.last_mut().unwrap()
    }
    fn switch(&mut self, key: &str, label: &str, detail: &str, on: bool) -> &mut PrefRow {
        let mut r = Self::base(1, key, label, detail);
        r.on = on;
        self.push(r)
    }
    fn choice(&mut self, key: &str, label: &str, detail: &str, options: &[&str], selected: usize) -> &mut PrefRow {
        let mut r = Self::base(2, key, label, detail);
        r.options = ModelRc::new(VecModel::from(options.iter().map(|o| SharedString::from(*o)).collect::<Vec<_>>()));
        r.selected = selected as i32;
        self.push(r)
    }
    fn finish(mut self) -> Vec<PrefRow> {
        if let Some(prev) = self.0.last_mut() {
            prev.last = true;
        }
        self.0
    }
}

fn zenity() -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join("zenity")).find(|p| p.is_file())
}

fn build_rows(s: &Settings, server: Option<&Value>, server_error: Option<&str>, private: Option<bool>, st: &PageState) -> Vec<PrefRow> {
    const RESTART: &str = "Applies after restart";
    let mut r = Rows(Vec::new());
    let srv_ok = server.is_some();
    let sb = |k: &str| server.and_then(|v| v.get(k)).and_then(Value::as_bool).unwrap_or(false);
    let sn = |k: &str| server.and_then(|v| v.get(k)).and_then(Value::as_u64).unwrap_or(0);
    let offline_detail = server_error.map(|e| format!("Server not reachable: {e}")).unwrap_or_else(|| "Loading…".into());

    r.group("Appearance", "");
    let theme = match (s.theme_source, s.light) {
        (ThemeSource::Desktop, _) => 0,
        (ThemeSource::Geonix, true) => 1,
        (ThemeSource::Geonix, false) => 2,
    };
    r.choice("theme", "Theme", "Desktop colours follow your wallpaper scheme.", &["Desktop colours", "Light", "Dark"], theme);
    r.switch("waves", "Animated waves", "The moving waves behind the player.", s.waves).enabled = !s.reduce_motion;
    r.switch("aurora", "Background animation", "The slowly drifting colour glow.", s.aurora).enabled = !s.reduce_motion;
    r.switch("cover_glow", "Cover glow", "Soft light behind the cover on Now playing.", s.cover_glow);
    r.switch("reduce_motion", "Reduce motion", "Turns off the wave and background animations.", s.reduce_motion);

    r.group("Playback", "");
    r.switch("gapless", "Gapless playback", "Loads the next song early so there is no pause between songs.", s.gapless);
    r.switch("autoplay_recs", "Keep playing", "When the queue ends, continue with recommendations.", s.autoplay_recs);
    r.switch("remember_volume", "Remember volume", "Start at the volume you last used.", s.remember_volume);
    {
        let v = st.volume_drag.unwrap_or(s.default_volume);
        let mut row = Rows::base(4, "default_volume", "Start volume", "Used when the app opens.");
        row.value = v;
        row.text = format!("{}%", (v * 100.0).round() as i32).into();
        row.enabled = !s.remember_volume;
        r.push(row);
    }
    r.switch("media_keys", "Media keys", "Play, pause and skip from the keyboard and the desktop media widget.", s.media_keys).note =
        RESTART.into();

    r.group("Lyrics", "");
    r.switch("show_lyrics", "Show lyrics", "Lyrics on Now playing.", s.show_lyrics);
    r.switch("karaoke", "Karaoke highlight", "Highlight the line being sung in synced lyrics.", s.karaoke).enabled = s.show_lyrics;

    r.group("Downloads", if srv_ok { "" } else { &offline_detail });
    {
        let mut row = Rows::base(
            3,
            "download_dir",
            "Download folder",
            "Where new songs are saved. Leave empty for the default. Press Enter to apply.",
        );
        row.text = st
            .dir_text
            .clone()
            .unwrap_or_else(|| server.and_then(|v| v.get("download_dir")).and_then(Value::as_str).unwrap_or("").to_string())
            .into();
        row.browse = zenity().is_some();
        row.enabled = srv_ok;
        r.push(row);
    }
    let q = KBPS.iter().position(|k| *k == sn("audio_kbps")).unwrap_or(0);
    r.choice("audio_kbps", "Audio quality", "MP3 bitrate for new downloads.", &KBPS_LABELS, q).enabled = srv_ok;
    let par = (sn("parallel_downloads").clamp(1, 4) - 1) as usize;
    r.choice("parallel_downloads", "Parallel downloads", "How many songs download at the same time.", &["1", "2", "3", "4"], par).enabled =
        srv_ok;
    r.switch("auto_retry", "Retry automatically", "Try again with another method when YouTube refuses a download.", sb("auto_retry"))
        .enabled = srv_ok;

    r.group("Recommendations", if srv_ok { "" } else { &offline_detail });
    let private_on = private.unwrap_or_else(|| sb("private_listening"));
    r.switch("private_listening", "Private listening", "What you play is not remembered and does not shape recommendations.", private_on)
        .enabled = srv_ok;
    r.switch("daily_mix", "Daily refresh", "Refresh recommendations once a day in the background.", sb("daily_mix")).enabled = srv_ok;
    r.switch(
        "discover_mix",
        "Discover Mix",
        "Keep the Discover Mix playlist up to date when recommendations refresh.",
        sb("discover_mix"),
    )
    .enabled = srv_ok;

    r.group("Sync", if srv_ok { "" } else { &offline_detail });
    r.switch("device_sync", "Device sync", "Share your library with your other computers over Tailscale.", sb("device_sync")).enabled =
        srv_ok;
    r.switch("auto_sync_on_open", "Sync when the app opens", "Sync with paired devices at start.", sb("auto_sync_on_open")).enabled =
        srv_ok && sb("device_sync");

    r.group("Phone page", if srv_ok { "" } else { &offline_detail });
    r.switch("phone_page", "Phone download page", "Serve the page your phone uses to download songs to this computer.", sb("phone_page"))
        .enabled = srv_ok;

    r.group("Performance", "");
    let fps = FPS_CHOICES.iter().position(|f| *f == s.max_fps).unwrap_or(FPS_CHOICES.len() - 1);
    r.choice("max_fps", "Animation frame rate", "Lower uses less CPU.", &["12 fps", "24 fps", "30 fps"], fps);
    {
        let gpu_detail = if cfg!(feature = "femtovg") {
            "Draw with OpenGL instead of the CPU."
        } else {
            "Draw with OpenGL instead of the CPU. This build has no GPU renderer."
        };
        r.switch("gpu_renderer", "GPU renderer", gpu_detail, s.gpu_renderer).note = RESTART.into();
    }
    let cc = COVER_CACHE_CHOICES.iter().position(|c| *c == s.cover_cache).unwrap_or(1);
    r.choice("cover_cache", "Cover cache", "Memory for decoded album covers.", &["12 MB", "24 MB", "48 MB", "96 MB"], cc);

    r.group("Data", "Export and import cover both app and server settings.");
    {
        let mut row = Rows::base(3, "file", "Settings file", "");
        row.text = st.file_text.clone().unwrap_or_else(|| default_file().to_string_lossy().into_owned()).into();
        row.browse = zenity().is_some();
        r.push(row);
    }
    {
        let mut row = Rows::base(5, "data", "Backup", "Export to the file above, or import and apply it.");
        row.options = ModelRc::new(VecModel::from(vec![
            SharedString::from("Export"),
            SharedString::from("Import"),
            SharedString::from("Reset to defaults"),
        ]));
        r.push(row);
    }
    r.finish()
}

// ------------------------------------------------------------ controller

impl App {
    /// Called once at startup: wires the callbacks and applies settings that
    /// live in Slint globals.
    pub fn prefs_init(&mut self) {
        let model = Rc::new(VecModel::<PrefRow>::default());
        self.win.global::<Prefs>().set_rows(ModelRc::from(model.clone()));
        PAGE.with(|p| p.borrow_mut().model = Some(model));
        let g = self.win.global::<Prefs>();
        g.on_toggle(|k| with_app(move |a| a.pref_toggle(&k)));
        g.on_choose(|k, i| with_app(move |a| a.pref_choose(&k, i as usize)));
        g.on_text_edited(|k, t| {
            PAGE.with(|p| {
                let mut p = p.borrow_mut();
                match k.as_str() {
                    "download_dir" => p.dir_text = Some(t.to_string()),
                    "file" => p.file_text = Some(t.to_string()),
                    _ => {}
                }
            })
        });
        g.on_text_accepted(|k, t| with_app(move |a| a.pref_text(&k, t.to_string())));
        g.on_browse(|k| with_app(move |a| a.pref_browse(k.to_string())));
        g.on_slider_moved(|_, v| {
            PAGE.with(|p| p.borrow_mut().volume_drag = Some(v));
            with_app(|a| a.refresh_prefs());
        });
        g.on_slider_released(|_, v| {
            with_app(move |a| {
                PAGE.with(|p| p.borrow_mut().volume_drag = None);
                a.settings.default_volume = v.clamp(0.0, 1.0);
                a.save_prefs("");
            })
        });
        self.apply_ui_prefs();
    }

    /// The Settings page was opened.
    pub fn settings_page_opened(&mut self) {
        self.set_status("", false);
        self.refresh_prefs();
        self.load_server_prefs();
    }

    fn load_server_prefs(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.server_settings().await;
            post(move |a| {
                PAGE.with(|p| {
                    let mut p = p.borrow_mut();
                    match r {
                        Ok(v) => {
                            p.server = Some(v);
                            p.server_error = None;
                        }
                        Err(e) => p.server_error = Some(e.message),
                    }
                });
                a.refresh_prefs();
            });
        });
    }

    fn set_status(&self, text: &str, error: bool) {
        let g = self.win.global::<Prefs>();
        g.set_status(text.into());
        g.set_status_error(error);
    }

    /// Rebuild the rows, updating only the ones that changed.
    pub fn refresh_prefs(&mut self) {
        let private = self.private_listening;
        let rows = PAGE.with(|p| {
            let p = p.borrow();
            build_rows(&self.settings, p.server.as_ref(), p.server_error.as_deref(), private, &p)
        });
        PAGE.with(|p| {
            let p = p.borrow();
            let Some(m) = p.model.as_ref() else { return };
            if m.row_count() != rows.len() {
                m.set_vec(rows);
                return;
            }
            for (i, r) in rows.into_iter().enumerate() {
                if m.row_data(i).as_ref() != Some(&r) {
                    m.set_row_data(i, r);
                }
            }
        });
    }

    /// Push UI settings into Slint globals, the player and the caches.
    fn apply_ui_prefs(&mut self) {
        let g = self.win.global::<Prefs>();
        g.set_cover_glow(self.settings.cover_glow);
        g.set_show_lyrics(self.settings.show_lyrics);
        g.set_karaoke(self.settings.karaoke);
        if !self.settings.show_lyrics {
            self.win.global::<crate::Player>().set_np_tab(0);
        }
        crate::covers::set_limit_mb(self.settings.cover_cache);
        self.settings.apply_global();
    }

    /// Save ui.json, apply, refresh the page, show `msg` if any.
    fn save_prefs(&mut self, msg: &str) {
        self.settings = self.settings.clone().sanitized();
        self.settings.save();
        self.apply_ui_prefs();
        self.refresh_prefs();
        if !msg.is_empty() {
            self.set_status(msg, false);
        }
    }

    fn pref_toggle(&mut self, key: &str) {
        let s = &mut self.settings;
        let flip = |b: &mut bool| *b = !*b;
        match key {
            "waves" => flip(&mut s.waves),
            "aurora" => flip(&mut s.aurora),
            "cover_glow" => flip(&mut s.cover_glow),
            "reduce_motion" => flip(&mut s.reduce_motion),
            "gapless" => flip(&mut s.gapless),
            "autoplay_recs" => flip(&mut s.autoplay_recs),
            "remember_volume" => flip(&mut s.remember_volume),
            "media_keys" => flip(&mut s.media_keys),
            "show_lyrics" => flip(&mut s.show_lyrics),
            "karaoke" => flip(&mut s.karaoke),
            "gpu_renderer" => flip(&mut s.gpu_renderer),
            server_key => {
                let cur = PAGE.with(|p| p.borrow().server.as_ref().and_then(|v| v.get(server_key)).and_then(Value::as_bool));
                let cur = if server_key == "private_listening" { self.private_listening.or(cur) } else { cur };
                if let Some(cur) = cur {
                    self.patch_server(json!({ server_key: !cur }));
                }
                return;
            }
        }
        self.save_prefs("");
        match key {
            "gapless" => self.update_preload(),
            "show_lyrics" if self.settings.show_lyrics => self.track_changed(),
            "show_lyrics" | "karaoke" => {
                let l = self.current_lyrics.clone();
                if l.is_some() || !self.settings.show_lyrics {
                    self.show_lyrics(l);
                }
            }
            "waves" | "aurora" | "reduce_motion" => self.render_static(),
            "media_keys" | "gpu_renderer" => self.set_status("Saved. This applies the next time you open the app.", false),
            _ => {}
        }
    }

    fn pref_choose(&mut self, key: &str, i: usize) {
        match key {
            "theme" => {
                let s = &mut self.settings;
                match i {
                    0 => s.theme_source = ThemeSource::Desktop,
                    1 => (s.theme_source, s.light) = (ThemeSource::Geonix, true),
                    _ => (s.theme_source, s.light) = (ThemeSource::Geonix, false),
                }
                self.save_prefs("");
                self.apply_theme();
            }
            "max_fps" => {
                self.settings.max_fps = FPS_CHOICES[i.min(FPS_CHOICES.len() - 1)];
                self.save_prefs("");
            }
            "cover_cache" => {
                self.settings.cover_cache = COVER_CACHE_CHOICES[i.min(COVER_CACHE_CHOICES.len() - 1)];
                self.save_prefs("");
            }
            "audio_kbps" => self.patch_server(json!({ "audio_kbps": KBPS[i.min(3)] })),
            "parallel_downloads" => self.patch_server(json!({ "parallel_downloads": i.min(3) + 1 })),
            "data" => match i {
                0 => self.export_settings(),
                1 => self.import_settings(),
                _ => self.reset_settings(),
            },
            _ => {}
        }
    }

    fn pref_text(&mut self, key: &str, text: String) {
        match key {
            "download_dir" => {
                PAGE.with(|p| p.borrow_mut().dir_text = Some(text.clone()));
                self.patch_server(json!({ "download_dir": text.trim() }));
            }
            "file" => PAGE.with(|p| p.borrow_mut().file_text = Some(text)),
            _ => {}
        }
    }

    fn pref_browse(&mut self, key: String) {
        let Some(zenity) = zenity() else { return };
        let current = PAGE.with(|p| {
            let p = p.borrow();
            if key == "file" {
                p.file_text.clone().unwrap_or_else(|| default_file().to_string_lossy().into_owned())
            } else {
                p.dir_text.clone().unwrap_or_default()
            }
        });
        let mut cmd = tokio::process::Command::new(zenity);
        cmd.arg("--file-selection");
        if key == "file" {
            cmd.arg("--title=Settings file").arg(format!("--filename={current}"));
        } else {
            cmd.arg("--directory").arg("--title=Download folder");
            if !current.is_empty() {
                cmd.arg(format!("--filename={}/", current.trim_end_matches('/')));
            }
        }
        self.spawn(async move {
            let Ok(out) = cmd.output().await else { return };
            let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !out.status.success() || picked.is_empty() {
                return;
            }
            post(move |a| {
                if key == "file" {
                    PAGE.with(|p| p.borrow_mut().file_text = Some(picked));
                    a.refresh_prefs();
                } else {
                    a.pref_text("download_dir", picked);
                }
            });
        });
    }

    /// PATCH server settings; the page shows the server's answer or its error.
    fn patch_server(&mut self, changes: Value) {
        // Optimistic: show the change at once.
        PAGE.with(|p| {
            if let (Some(Value::Object(cur)), Value::Object(ch)) = (p.borrow_mut().server.as_mut(), &changes) {
                for (k, v) in ch {
                    cur.insert(k.clone(), v.clone());
                }
            }
        });
        if let Some(v) = changes.get("private_listening").and_then(Value::as_bool) {
            self.private_listening = Some(v);
            self.win.global::<crate::Lib>().set_private_listening(if v { 2 } else { 1 });
        }
        self.refresh_prefs();
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.patch_settings(changes.clone()).await;
            post(move |a| match r {
                Ok(v) => {
                    PAGE.with(|p| {
                        let mut p = p.borrow_mut();
                        p.server = Some(v.clone());
                        p.server_error = None;
                        if changes.get("download_dir").is_some() {
                            p.dir_text = None;
                        }
                    });
                    a.server_prefs_applied(&changes, &v);
                    a.refresh_prefs();
                    a.set_status("Saved", false);
                }
                Err(e) => {
                    a.set_status(&format!("Could not save: {}", e.message), true);
                    a.load_server_prefs();
                    a.load_private_listening(1);
                }
            });
        });
    }

    /// Side effects in the app of server-side changes.
    fn server_prefs_applied(&mut self, changes: &Value, all: &Value) {
        if let Some(v) = all.get("private_listening").and_then(Value::as_bool) {
            self.private_listening = Some(v);
            self.win.global::<crate::Lib>().set_private_listening(if v { 2 } else { 1 });
        }
        if let Some(on) = changes.get("device_sync").and_then(Value::as_bool) {
            if on {
                self.sync.available = None;
                self.update_devices_ui();
                self.sync_startup(1);
            } else {
                self.sync.available = Some(false);
                self.update_devices_ui();
            }
        }
    }

    fn file_path(&self) -> PathBuf {
        PAGE.with(|p| p.borrow().file_text.clone()).map(|t| expand(&t)).unwrap_or_else(default_file)
    }

    fn export_settings(&mut self) {
        let path = self.file_path();
        let ui = self.settings.clone();
        let api = self.api.clone();
        self.spawn(async move {
            let server = api.server_settings().await.ok();
            let text = build_export(&ui, server.as_ref());
            let r = std::fs::write(&path, text);
            post(move |a| match r {
                Ok(()) => {
                    let msg = if server.is_some() {
                        format!("Exported to {}", path.display())
                    } else {
                        format!("Exported app settings to {} (server not reachable, its settings are missing)", path.display())
                    };
                    a.set_status(&msg, false);
                    a.snack("Settings exported");
                }
                Err(e) => a.set_status(&format!("Could not write {}: {e}", path.display()), true),
            });
        });
    }

    fn import_settings(&mut self) {
        let path = self.file_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("Could not read {}: {e}", path.display()), true);
                return;
            }
        };
        let imported = match parse_export(&text) {
            Ok(i) => i,
            Err(e) => {
                self.set_status(&format!("Import failed: {e}. Nothing was changed."), true);
                return;
            }
        };
        self.apply_imported(imported, format!("Imported settings from {}", path.display()));
    }

    fn reset_settings(&mut self) {
        let mut ui = Settings::default();
        ui.volume = self.settings.volume; // keep the current loudness
        let server = server_defaults().as_object().cloned();
        self.apply_imported(Imported { ui: Some(ui), server }, "Settings reset to defaults".into());
    }

    /// Server part first (it may refuse), then the app part.
    fn apply_imported(&mut self, imported: Imported, msg: String) {
        let api = self.api.clone();
        self.spawn(async move {
            let server_result = match &imported.server {
                Some(m) => Some(api.patch_settings(Value::Object(m.clone())).await),
                None => None,
            };
            post(move |a| {
                let changes = imported.server.clone().map(Value::Object).unwrap_or(Value::Null);
                match server_result {
                    Some(Err(e)) => {
                        a.set_status(&format!("Import failed: the server refused it ({}). Nothing was changed.", e.message), true);
                        return;
                    }
                    Some(Ok(v)) => {
                        PAGE.with(|p| {
                            let mut p = p.borrow_mut();
                            p.server = Some(v.clone());
                            p.server_error = None;
                            p.dir_text = None;
                        });
                        a.server_prefs_applied(&changes, &v);
                    }
                    None => {}
                }
                if let Some(ui) = imported.ui {
                    let old = std::mem::replace(&mut a.settings, ui);
                    a.save_prefs("");
                    a.apply_theme();
                    a.update_preload();
                    let l = a.current_lyrics.clone();
                    a.show_lyrics(l);
                    if (old.volume - a.settings.volume).abs() > f32::EPSILON {
                        a.set_volume(a.settings.volume * 100.0);
                    }
                    a.render_static();
                }
                a.refresh_prefs();
                a.set_status(&msg, false);
                a.snack(msg.clone());
            });
        });
    }

    /// Queue ended: continue with recommendations if that is turned on.
    pub fn continue_with_recs(&mut self) -> bool {
        if !self.settings.autoplay_recs {
            return false;
        }
        let have: std::collections::HashSet<String> = self.queue.tracks.iter().map(|t| t.video_id.clone()).collect();
        let more: Vec<_> =
            self.recs.as_ref().map(|r| r.iter().filter(|t| !have.contains(&t.video_id)).take(10).cloned().collect()).unwrap_or_default();
        if more.is_empty() {
            return false;
        }
        self.queue.tracks.extend(more.iter().cloned());
        self.queue.original.extend(more);
        self.next(false);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_import_round_trip() {
        let ui = Settings { waves: false, max_fps: 12, gpu_renderer: true, cover_cache: 48, light: true, ..Default::default() };
        let mut server = server_defaults();
        server["audio_kbps"] = json!(192);
        server["phone_page"] = json!(false);
        let text = build_export(&ui, Some(&server));
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], EXPORT_VERSION);
        let back = parse_export(&text).unwrap();
        assert_eq!(back.ui.unwrap(), ui);
        assert_eq!(Value::Object(back.server.unwrap()), server);
    }

    #[test]
    fn export_without_server_imports_app_part() {
        let text = build_export(&Settings::default(), None);
        let back = parse_export(&text).unwrap();
        assert!(back.server.is_none());
        assert_eq!(back.ui.unwrap(), Settings::default());
    }

    #[test]
    fn invalid_files_are_rejected_with_a_reason() {
        let cases = [
            ("", "not a valid JSON"),
            ("[1,2]", "not a Music Player Lite settings file"),
            (r#"{"ui": {}}"#, "no version"),
            (r#"{"version": "1", "ui": {}}"#, "whole number"),
            (r#"{"version": 99, "ui": {}}"#, "not supported"),
            (r#"{"version": 1}"#, "no settings"),
            (r#"{"version": 1, "app": "other"}"#, "not from Music Player Lite"),
            (r#"{"version": 1, "ui": {"waves": "yes"}}"#, "ui.waves"),
            (r#"{"version": 1, "ui": {"nonsense": 1}}"#, "unknown setting"),
            (r#"{"version": 1, "ui": {"max_fps": 999}}"#, "max_fps"),
            (r#"{"version": 1, "ui": {"volume": 7}}"#, "between 0 and 1"),
            (r#"{"version": 1, "ui": []}"#, "must be an object"),
            (r#"{"version": 1, "server": {"audio_kbps": 64}}"#, "audio_kbps"),
            (r#"{"version": 1, "server": {"parallel_downloads": 8}}"#, "parallel_downloads"),
            (r#"{"version": 1, "server": {"phone_page": 1}}"#, "server.phone_page"),
            (r#"{"version": 1, "server": {"rm_rf": true}}"#, "unknown setting"),
            (r#"{"version": 1, "extra": {}}"#, "unknown section"),
        ];
        for (text, needle) in cases {
            let err = parse_export(text).expect_err(text);
            assert!(err.contains(needle), "{text}: {err}");
        }
    }

    #[cfg(feature = "embedded-server")]
    #[test]
    fn server_defaults_match_the_server() {
        let real = serde_json::to_value(geonix_server::settings::ServerSettings::default()).unwrap();
        assert_eq!(real, server_defaults());
    }

    #[test]
    fn reduce_motion_turns_off_animations_for_readers() {
        let s = Settings { reduce_motion: true, ..Default::default() };
        let v = s.visual();
        assert!(!v.waves_enabled && !v.aurora_enabled && v.reduce_motion);
        let v = Settings::default().visual();
        assert!(v.waves_enabled && v.aurora_enabled && v.max_fps == 30 && !v.gpu_renderer);
    }

    #[test]
    fn rows_cover_every_group() {
        let st = PageState::default();
        let rows = build_rows(&Settings::default(), Some(&server_defaults()), None, None, &st);
        let groups: Vec<String> = rows.iter().filter(|r| r.kind == 0).map(|r| r.label.to_string()).collect();
        assert_eq!(
            groups,
            ["Appearance", "Playback", "Lyrics", "Downloads", "Recommendations", "Sync", "Phone page", "Performance", "Data"]
        );
        // Server rows are disabled while the server is unknown.
        let rows = build_rows(&Settings::default(), None, Some("offline"), None, &st);
        assert!(!rows.iter().find(|r| r.key == "phone_page").unwrap().enabled);
        assert!(rows.iter().find(|r| r.key == "waves").unwrap().enabled);
    }
}
