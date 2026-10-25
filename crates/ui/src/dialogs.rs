//! Menus, dialogs, playlist actions, playlist import, device sync and
//! keyboard shortcuts (state/actions.dart, widgets/*dialog*.dart, state/sync.dart).

use std::collections::HashSet;
use std::time::Duration;

use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel};

use crate::api::{friendly_import_error, looks_like_playlist_url, Playlist, PlaylistPreview, SyncEvent, SyncStatus, Track};
use crate::app::{plural, post, with_app, App, DialogCtx, MenuCtx, Route};
use crate::settings::ThemeSource;
use crate::{DeviceItem, Icons, MenuEntry, PlaylistItem, PreviewItem};

const SORTS: [(&str, &str); 6] = [
    ("added", "Date added"),
    ("title", "Title"),
    ("artist", "Artist"),
    ("album", "Album"),
    ("plays", "Most played"),
    ("recent", "Recently played"),
];

fn entry(icon: Image, label: &str) -> MenuEntry {
    MenuEntry { icon, label: label.into(), subtitle: SharedString::new(), enabled: true, checked: false, divider: false }
}

fn divider() -> MenuEntry {
    MenuEntry { divider: true, ..Default::default() }
}

impl App {
    fn open_menu(&mut self, ctx: MenuCtx, items: Vec<MenuEntry>, x: f32, y: f32, width: f32) {
        self.menu = ctx;
        let ui = self.win.global::<crate::Ui>();
        ui.set_menu_items(ModelRc::new(VecModel::from(items)));
        ui.set_menu_x(x);
        ui.set_menu_y(y);
        ui.set_menu_width(width);
        ui.set_menu_open(true);
    }

    pub fn open_track_menu(&mut self, list: String, index: usize, x: f32, y: f32) {
        let Some(t) = self.list(&list).get(index).cloned() else { return };
        let icons = self.win.global::<Icons>();
        let mut items = vec![
            entry(icons.get_queue_play_next(), "Play next"),
            entry(icons.get_playlist_play(), "Add to queue"),
            entry(icons.get_playlist_add(), "Add to playlist…"),
        ];
        if !t.downloaded {
            items.push(entry(icons.get_download(), "Download"));
        }
        let in_playlist = if list == "playlist" { self.playlist.as_ref().filter(|p| !p.is_discover()).map(|p| p.id) } else { None };
        if in_playlist.is_some() {
            items.push(entry(icons.get_remove_circle(), "Remove from this playlist"));
        }
        if t.downloaded {
            items.push(entry(icons.get_delete_outline(), "Remove from library"));
        }
        let h = items.len() as f32 * 48.0 + 16.0;
        // open to the left of the ⋯ button, flipping up near the bottom
        self.open_menu(
            MenuCtx::Track(t, in_playlist),
            items,
            x - 230.0,
            if y + h > self.win.global::<crate::Theme>().get_win_height() - 8.0 { y - 40.0 - h } else { y },
            230.0,
        );
    }

    pub fn open_theme_menu(&mut self, x: f32, y: f32) {
        let icons = self.win.global::<Icons>();
        let found = self.desktop.is_some();
        let following = self.following_desktop();
        let s = &self.settings;
        let mut desk = entry(icons.get_wallpaper(), "Desktop colors");
        desk.subtitle = if found { "Matches your wallpaper".into() } else { "No desktop color scheme found".into() };
        desk.checked = s.theme_source == ThemeSource::Desktop;
        desk.enabled = found;
        let mut gx = entry(icons.get_palette(), "Default purple");
        gx.checked = s.theme_source == ThemeSource::Geonix || !found;
        let mut mode =
            entry(if s.light { icons.get_dark_mode() } else { icons.get_light_mode() }, if s.light { "Dark mode" } else { "Light mode" });
        if following {
            mode.subtitle = "Follows the desktop".into();
            mode.enabled = false;
        }
        let items = vec![desk, gx, divider(), mode];
        self.open_menu(MenuCtx::Theme, items, x, y - 190.0, 260.0);
    }

    pub fn open_sort_menu(&mut self, x: f32, y: f32) {
        let current = self.win.global::<crate::Lib>().get_library_sort().to_string();
        let items = SORTS
            .iter()
            .map(|(k, l)| {
                let mut e = entry(Image::default(), l);
                e.checked = *k == current;
                e
            })
            .collect();
        self.open_menu(MenuCtx::Sort, items, x, y + 4.0, 220.0);
    }

    pub fn open_playlist_menu(&mut self, x: f32, y: f32) {
        let icons = self.win.global::<Icons>();
        let items = vec![
            entry(icons.get_playlist_add(), "Import from YouTube Music"),
            divider(),
            entry(Image::default(), "Rename"),
            entry(Image::default(), "Delete playlist"),
        ];
        self.open_menu(MenuCtx::Playlist, items, x, y, 280.0);
    }

    pub fn menu_selected(&mut self, i: i32) {
        // 100: "Import from YouTube Music" button of an empty playlist
        if i == 100 {
            if let Some(p) = self.playlist.clone() {
                self.open_import_into(p);
            }
            return;
        }
        let ctx = std::mem::replace(&mut self.menu, MenuCtx::None);
        match ctx {
            MenuCtx::None => {}
            MenuCtx::Track(t, in_pl) => {
                let mut actions: Vec<&str> = vec!["next", "queue", "playlist"];
                if !t.downloaded {
                    actions.push("download");
                }
                if in_pl.is_some() {
                    actions.push("remove-pl");
                }
                if t.downloaded {
                    actions.push("remove-lib");
                }
                match actions.get(i as usize).copied() {
                    Some("next") => self.play_next(t),
                    Some("queue") => self.add_to_queue(t),
                    Some("playlist") => self.add_to_playlist(t),
                    Some("download") => self.download(t),
                    Some("remove-pl") => self.remove_from_playlist(in_pl.unwrap(), t),
                    Some("remove-lib") => self.confirm_remove_track(t),
                    _ => {}
                }
            }
            MenuCtx::Theme => {
                match i {
                    0 => self.settings.theme_source = ThemeSource::Desktop,
                    1 => self.settings.theme_source = ThemeSource::Geonix,
                    3 => self.settings.light = !self.settings.light,
                    _ => return,
                }
                self.settings.save();
                self.apply_theme();
                self.refresh_prefs();
            }
            MenuCtx::Sort => {
                if let Some((k, l)) = SORTS.get(i as usize) {
                    let lib = self.win.global::<crate::Lib>();
                    lib.set_library_sort((*k).into());
                    lib.set_library_sort_label((*l).into());
                    self.reload_library_tracks();
                }
            }
            MenuCtx::Playlist => {
                let Some(p) = self.playlist.clone() else { return };
                match i {
                    0 => self.open_import_into(p),
                    2 => {
                        self.dialog = DialogCtx::RenamePlaylist(p.clone());
                        let ui = self.win.global::<crate::Ui>();
                        ui.set_dlg_title("Rename playlist".into());
                        ui.set_dlg_text(p.name.as_str().into());
                        ui.set_dlg_hint("Playlist name".into());
                        ui.set_dlg_confirm("Save".into());
                        ui.set_dialog("text".into());
                    }
                    3 => {
                        let ui = self.win.global::<crate::Ui>();
                        ui.set_dlg_title(format!("Delete “{}”?", p.name).into());
                        ui.set_dlg_message("The playlist is removed. Your songs and music files stay in your library.".into());
                        ui.set_dlg_confirm("Delete playlist".into());
                        ui.set_dialog("confirm".into());
                        self.dialog = DialogCtx::DeletePlaylist(p);
                    }
                    _ => {}
                }
            }
        }
    }

    // ------------------------------------------------------------ dialogs

    pub fn close_dialog(&mut self) {
        self.dialog = DialogCtx::None;
        self.dev_timer = None;
        self.import.seq += 1;
        self.win.global::<crate::Ui>().set_dialog(SharedString::new());
        self.win.invoke_focus_keys();
    }

    pub fn dialog_cancel(&mut self) {
        // Removing a device returns to the devices dialog.
        if let DialogCtx::RemoveDevice(_) = self.dialog {
            self.open_devices();
            return;
        }
        self.close_dialog();
    }

    pub fn dialog_confirm(&mut self) {
        let ctx = std::mem::replace(&mut self.dialog, DialogCtx::None);
        let ui = self.win.global::<crate::Ui>();
        match ctx {
            DialogCtx::DeletePlaylist(p) => {
                self.close_dialog();
                let api = self.api.clone();
                self.spawn(async move {
                    let r = api.delete_playlist(p.id).await;
                    post(move |a| match r {
                        Ok(()) => {
                            a.reload_playlists();
                            a.snack(format!("Deleted “{}”", p.name));
                            a.win.global::<crate::Lib>().set_library_tab(3);
                            a.go(Route::Library);
                        }
                        Err(e) => a.snack(format!("Could not delete: {e}")),
                    });
                });
            }
            DialogCtx::RenamePlaylist(p) => {
                let name = ui.get_dlg_text().trim().to_string();
                self.close_dialog();
                if name.is_empty() || name == p.name {
                    return;
                }
                let api = self.api.clone();
                self.spawn(async move {
                    let r = api.rename_playlist(p.id, &name).await;
                    post(move |a| match r {
                        Ok(()) => a.reload_playlists(),
                        Err(e) => a.snack(format!("Could not rename: {e}")),
                    });
                });
            }
            DialogCtx::RemoveTrack(t) => {
                let delete_file = ui.get_dlg_check();
                self.close_dialog();
                let api = self.api.clone();
                self.spawn(async move {
                    let r = api.delete_track(&t.video_id, delete_file).await;
                    post(move |a| match r {
                        Ok(()) => {
                            a.refresh_all();
                            a.snack(if delete_file { "Removed and deleted the file" } else { "Removed from your library" });
                        }
                        Err(e) => a.snack(format!("Could not remove: {e}")),
                    });
                });
            }
            DialogCtx::NewPlaylist { allow_import, then_add } => {
                if !ui.get_import_can_submit() {
                    self.dialog = DialogCtx::NewPlaylist { allow_import, then_add };
                    return;
                }
                let importing = ui.get_np_import();
                let typed = ui.get_np_name().trim().to_string();
                let preview_title = self.import.preview.as_ref().map(|p| p.title.clone()).unwrap_or_default();
                let name = if !typed.is_empty() { typed } else { preview_title };
                let url = self.import.url.clone();
                self.close_dialog();
                self.create_playlist(name, if importing { Some(url) } else { None }, then_add);
            }
            DialogCtx::ImportInto(p) => {
                if !ui.get_import_can_submit() {
                    self.dialog = DialogCtx::ImportInto(p);
                    return;
                }
                let url = self.import.url.clone();
                self.close_dialog();
                let api = self.api.clone();
                self.spawn(async move {
                    let r = api.download_playlist(&url, None, Some(p.id)).await;
                    post(move |a| match r {
                        Ok(r) => {
                            a.reload_playlists();
                            let added = r.queued + r.skipped;
                            a.snack(if added == 0 {
                                format!("Nothing new — every song is already in “{}”", p.name)
                            } else {
                                format!(
                                    "Added {} to “{}”{}{}",
                                    plural(added, "song"),
                                    p.name,
                                    if r.queued > 0 { format!(", downloading {}", r.queued) } else { String::new() },
                                    if r.already_in_playlist > 0 {
                                        format!(" ({} already there)", r.already_in_playlist)
                                    } else {
                                        String::new()
                                    }
                                )
                            });
                        }
                        Err(e) => a.snack(friendly_import_error(&e)),
                    });
                });
            }
            DialogCtx::RemoveDevice(id) => {
                self.remove_device(id);
                self.open_devices();
            }
            other => self.dialog = other,
        }
    }

    pub fn confirm_remove_track(&mut self, t: Track) {
        let ui = self.win.global::<crate::Ui>();
        ui.set_dlg_message(format!("“{}” will be removed from your library and all playlists.", t.title).into());
        ui.set_dlg_check(false);
        ui.set_dialog("remove-track".into());
        self.dialog = DialogCtx::RemoveTrack(t);
    }

    pub fn add_to_playlist(&mut self, t: Track) {
        let lists: Vec<Playlist> = self.playlists.clone().unwrap_or_default().into_iter().filter(|p| !p.is_discover()).collect();
        let mut items = vec![PlaylistItem { id: -1, name: "New playlist".into(), ..Default::default() }];
        items.extend(lists.iter().map(|p| PlaylistItem {
            id: p.id as i32,
            name: p.name.as_str().into(),
            subtitle: format!("{} songs", p.track_count).into(),
            discover: false,
            covers: ModelRc::new(VecModel::from(p.cover_track_ids.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>())),
        }));
        let ui = self.win.global::<crate::Ui>();
        ui.set_picker_items(ModelRc::new(VecModel::from(items)));
        ui.set_dialog("picker".into());
        self.dialog = DialogCtx::Picker(t, lists);
    }

    pub fn picker_chosen(&mut self, i: i32) {
        let DialogCtx::Picker(t, lists) = std::mem::replace(&mut self.dialog, DialogCtx::None) else { return };
        if i == 0 {
            self.open_new_playlist(false, Some(t));
            return;
        }
        self.close_dialog();
        if let Some(p) = lists.get(i as usize - 1).cloned() {
            self.add_track_to(p.id, p.name, t);
        }
    }

    fn add_track_to(&mut self, id: i64, name: String, t: Track) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.add_to_playlist(id, &t.video_id).await;
            post(move |a| match r {
                Ok(()) => {
                    a.reload_playlists();
                    a.snack(format!("Added to “{name}”"));
                }
                Err(e) => a.snack(format!("Could not add: {e}")),
            });
        });
    }

    pub fn remove_from_playlist(&mut self, id: i64, t: Track) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.remove_from_playlist(id, &t.video_id).await;
            post(move |a| match r {
                Ok(()) => a.reload_playlists(),
                Err(e) => a.snack(format!("Could not remove: {e}")),
            });
        });
    }

    pub fn open_new_playlist(&mut self, allow_import: bool, then_add: Option<Track>) {
        self.reset_import(None);
        let ui = self.win.global::<crate::Ui>();
        ui.set_np_name(SharedString::new());
        ui.set_np_import(false);
        ui.set_dlg_check(allow_import);
        ui.set_import_into(false);
        self.dialog = DialogCtx::NewPlaylist { allow_import, then_add };
        self.update_import_ui();
        self.win.global::<crate::Ui>().set_dialog("new-playlist".into());
    }

    pub fn open_import_into(&mut self, p: Playlist) {
        self.reset_import(Some(p.tracks.iter().map(|t| t.video_id.clone()).collect()));
        let ui = self.win.global::<crate::Ui>();
        ui.set_dlg_title(format!("Import into “{}”", p.name).into());
        ui.set_import_into(true);
        ui.set_np_import(true);
        self.dialog = DialogCtx::ImportInto(p);
        self.update_import_ui();
        self.win.global::<crate::Ui>().set_dialog("import".into());
    }

    fn reset_import(&mut self, existing: Option<HashSet<String>>) {
        self.import.seq += 1;
        self.import.url.clear();
        self.import.preview = None;
        self.import.existing = existing;
        self.import.name_edited = false;
        let ui = self.win.global::<crate::Ui>();
        ui.set_import_url(SharedString::new());
        ui.set_import_state(0);
        ui.set_import_error(SharedString::new());
        ui.set_import_items(ModelRc::default());
    }

    pub fn np_name_edited(&mut self, name: String) {
        self.win.global::<crate::Ui>().set_np_name(name.as_str().into());
        self.import.name_edited = true;
        self.update_import_ui();
    }

    pub fn import_url_edited(&mut self, value: String) {
        let ui = self.win.global::<crate::Ui>();
        ui.set_import_url(value.as_str().into());
        let url = value.trim().to_string();
        if url == self.import.url && self.import.preview.is_some() {
            self.update_import_ui();
            return;
        }
        self.import.seq += 1;
        let seq = self.import.seq;
        self.import.url = url.clone();
        self.import.preview = None;
        let valid = !url.is_empty() && looks_like_playlist_url(&url);
        ui.set_import_state(if valid {
            1
        } else if url.is_empty() {
            0
        } else {
            2
        });
        if !valid && !url.is_empty() {
            ui.set_import_error("That doesn’t look like a playlist link. It should contain “list=”.".into());
        }
        self.update_import_ui();
        if !valid {
            return;
        }
        let api = self.api.clone();
        self.spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let r = api.playlist_info(&url).await;
            post(move |a| {
                if seq != a.import.seq {
                    return;
                }
                match r {
                    Ok(p) if p.tracks.is_empty() => {
                        a.win.global::<crate::Ui>().set_import_state(2);
                        a.win.global::<crate::Ui>().set_import_error("That playlist has no songs that can be downloaded.".into());
                    }
                    Ok(p) => a.show_import_preview(p),
                    Err(e) => {
                        a.win.global::<crate::Ui>().set_import_state(2);
                        a.win.global::<crate::Ui>().set_import_error(friendly_import_error(&e).into());
                    }
                }
                a.update_import_ui();
            });
        });
    }

    fn show_import_preview(&mut self, p: PlaylistPreview) {
        let existing = self.import.existing.clone();
        let items: Vec<PreviewItem> = p
            .tracks
            .iter()
            .map(|t| PreviewItem {
                id: t.video_id.as_str().into(),
                title: t.title.as_str().into(),
                artist: t.artist.as_str().into(),
                status: status_of(t, existing.as_ref()),
            })
            .collect();
        let ui = self.win.global::<crate::Ui>();
        ui.set_import_items(ModelRc::new(VecModel::from(items)));
        ui.set_import_state(3);
        if matches!(self.dialog, DialogCtx::NewPlaylist { .. }) && (!self.import.name_edited || ui.get_np_name().trim().is_empty()) {
            ui.set_np_name(p.title.as_str().into());
            self.import.name_edited = false;
        }
        self.import.preview = Some(p);
    }

    /// Summary, button label and enabled state of the import dialogs.
    pub fn update_import_ui(&mut self) {
        let ui = self.win.global::<crate::Ui>();
        let counts = self.import.preview.as_ref().map(|p| counts(p, self.import.existing.as_ref()));
        if let (Some(c), Some(p)) = (counts, &self.import.preview) {
            let mut parts = vec![plural(p.tracks.len() as i64, "song")];
            if self.import.existing.is_some() {
                parts.push(format!("{} already in this playlist", c.0));
            }
            parts.push(format!("{} {}in library", c.1, if self.import.existing.is_none() { "already " } else { "" }));
            parts.push(format!("{} to download", c.2));
            ui.set_import_summary(parts.join(" · ").into());
            ui.set_import_unmatched(ModelRc::new(VecModel::from(
                p.unmatched.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>(),
            )));
            ui.set_import_note(p.note.clone().unwrap_or_default().into());
        }
        match &self.dialog {
            DialogCtx::NewPlaylist { .. } => {
                let importing = ui.get_np_import();
                let can = if importing { self.import.preview.is_some() } else { !ui.get_np_name().trim().is_empty() };
                ui.set_import_can_submit(can);
                ui.set_import_action(
                    match (importing, counts) {
                        (true, Some(c)) if c.2 == 0 => "Create playlist".to_string(),
                        (true, Some(c)) => format!("Create & download {}", c.2),
                        _ => "Create".to_string(),
                    }
                    .into(),
                );
            }
            DialogCtx::ImportInto(_) => {
                let to_add = counts.map(|c| c.1 + c.2).unwrap_or(0);
                ui.set_import_can_submit(to_add > 0);
                ui.set_import_action(
                    match counts {
                        None => "Add songs".to_string(),
                        Some(_) if to_add == 0 => "Nothing new to add".to_string(),
                        Some(c) if c.2 == 0 => format!("Add {}", plural(to_add, "song")),
                        Some(c) => format!("Add & download {}", c.2),
                    }
                    .into(),
                );
            }
            _ => {}
        }
    }

    pub fn paste_url(&mut self) {
        let w = self.win.as_weak();
        // Read the clipboard through a hidden TextInput-less path: Slint has no
        // clipboard API, so ask the platform via arboard-free fallback (wl-paste/xclip).
        self.spawn(async move {
            let out = tokio::process::Command::new("wl-paste").arg("-n").output().await;
            let text = match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
                _ => tokio::process::Command::new("xclip")
                    .args(["-o", "-selection", "clipboard"])
                    .output()
                    .await
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default(),
            };
            if !text.is_empty() {
                post(move |a| a.import_url_edited(text));
            }
            drop(w);
        });
    }

    fn create_playlist(&mut self, name: String, url: Option<String>, then_add: Option<Track>) {
        let api = self.api.clone();
        self.spawn(async move {
            match url {
                None => {
                    if name.is_empty() {
                        return;
                    }
                    let r = api.create_playlist(&name).await;
                    post(move |a| match r {
                        Ok(p) => {
                            a.reload_playlists();
                            match then_add {
                                Some(t) => a.add_track_to(p.id, p.name, t),
                                None => a.go(Route::Playlist(p.id)),
                            }
                        }
                        Err(e) => a.snack(format!("Could not create playlist: {e}")),
                    });
                }
                Some(url) => {
                    let r = api.download_playlist(&url, Some(&name), None).await;
                    post(move |a| match r {
                        Ok(r) => {
                            a.reload_playlists();
                            a.snack(if r.queued == 0 {
                                format!("Created “{name}”. Every song was already in your library")
                            } else {
                                format!(
                                    "Created “{name}”. Downloading {}{}",
                                    plural(r.queued, "song"),
                                    if r.skipped > 0 { format!(" ({} already in library)", r.skipped) } else { String::new() }
                                )
                            });
                            a.go(Route::Playlist(r.playlist_id));
                        }
                        Err(e) => a.snack(friendly_import_error(&e)),
                    });
                }
            }
        });
    }

    // ------------------------------------------------------------ device sync

    pub fn open_devices(&mut self) {
        self.dialog = DialogCtx::Devices;
        self.win.global::<crate::Ui>().set_pair_error(SharedString::new());
        self.update_devices_ui();
        self.win.global::<crate::Ui>().set_dialog("devices".into());
        self.load_sync();
        // Refresh the online dots while the dialog is open.
        let t = slint::Timer::default();
        t.start(slint::TimerMode::Repeated, Duration::from_secs(5), || with_app(|a| a.load_sync()));
        self.dev_timer = Some(t);
    }

    pub fn load_sync(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.sync_devices().await;
            post(move |a| a.sync_loaded(r));
        });
    }

    fn sync_loaded(&mut self, r: Result<SyncStatus, crate::api::ApiError>) {
        match r {
            Ok(s) => {
                let was = self.sync.running;
                self.sync.available = Some(true);
                let syncing = s.syncing;
                self.sync.status = Some(s);
                if syncing {
                    self.sync.running = true;
                } else if was && self.sync.settle.is_none() {
                    self.finish_sync();
                }
            }
            Err(e) => {
                if e.status == 404 || e.status == 405 {
                    self.sync.available = Some(false);
                } else {
                    self.win.global::<crate::Ui>().set_dev_error(e.to_string().into());
                }
            }
        }
        self.update_devices_ui();
    }

    pub fn update_devices_ui(&mut self) {
        let lib = self.win.global::<crate::Lib>();
        lib.set_sync_available(match self.sync.available {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        });
        lib.set_syncing(self.sync.running);
        let ui = self.win.global::<crate::Ui>();
        ui.set_dev_state(match (self.sync.available, &self.sync.status) {
            (Some(false), _) => 3,
            (_, Some(_)) => 1,
            _ if !ui.get_dev_error().is_empty() => 2,
            _ => 0,
        });
        if let Some(s) = &self.sync.status {
            let me = &s.this_device;
            ui.set_dev_this(if me.name.is_empty() { "This device".into() } else { format!("This device · {}", me.name).into() });
            ui.set_dev_code(me.code.as_str().into());
            let items: Vec<DeviceItem> = s
                .devices
                .iter()
                .map(|d| {
                    let synced = match d.last_sync_at.as_ref().and_then(parse_time) {
                        Some(t) => format!("Synced {}", format_ago(t)),
                        None => "Never synced".into(),
                    };
                    let err = d.last_error.as_ref().filter(|e| !e.trim().is_empty());
                    DeviceItem {
                        id: d.id.as_str().into(),
                        name: if d.name.is_empty() { d.address.as_str().into() } else { d.name.as_str().into() },
                        online: d.online,
                        detail: match err {
                            Some(e) => format!("{synced} · {e}").into(),
                            None => synced.into(),
                        },
                        error: err.is_some(),
                    }
                })
                .collect();
            ui.set_devices(ModelRc::new(VecModel::from(items)));
        }
    }

    pub fn sync_startup(&mut self, attempts: u32) {
        let api = self.api.clone();
        self.spawn(async move {
            for _ in 0..attempts {
                match api.sync_devices().await {
                    Ok(s) => {
                        // Settings: device sync on/off and auto-sync on open.
                        let prefs = api.server_settings().await.ok();
                        let flag = |k: &str| prefs.as_ref().and_then(|p| p.get(k)).and_then(|v| v.as_bool()).unwrap_or(true);
                        let (sync_on, auto) = (flag("device_sync"), flag("auto_sync_on_open"));
                        post(move |a| {
                            if !sync_on {
                                a.sync.available = Some(false);
                                a.update_devices_ui();
                                return;
                            }
                            let has = !s.devices.is_empty() && auto;
                            let syncing = s.syncing;
                            a.sync_loaded(Ok(s));
                            if syncing {
                                a.begin_sync(true);
                            } else if has {
                                a.run_sync(true);
                            }
                        });
                        return;
                    }
                    Err(e) if e.status == 404 || e.status == 405 => {
                        post(|a| {
                            a.sync.available = Some(false);
                            a.update_devices_ui();
                        });
                        return;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_secs(3)).await,
                }
            }
        });
    }

    fn begin_sync(&mut self, silent: bool) {
        self.sync.silent = silent;
        self.sync.new_songs = 0;
        self.sync.errors.clear();
        self.sync.running = true;
        let ui = self.win.global::<crate::Ui>();
        ui.set_sync_progress_visible(true);
        ui.set_sync_label("Starting sync…".into());
        ui.set_sync_label_error(false);
        ui.set_sync_fraction(-1.0);
        self.update_devices_ui();
    }

    pub fn run_sync(&mut self, silent: bool) {
        if self.sync.running || self.sync.available == Some(false) {
            return;
        }
        self.begin_sync(silent);
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.run_sync().await;
            post(move |a| {
                if let Err(e) = r {
                    if e.status == 409 {
                        return;
                    }
                    a.sync.running = false;
                    a.win.global::<crate::Ui>().set_sync_progress_visible(false);
                    a.update_devices_ui();
                    if !silent {
                        a.snack(format!("Sync failed: {e}"));
                    }
                }
            });
        });
    }

    pub fn on_sync_event(&mut self, e: SyncEvent) {
        if !self.sync.running {
            // Started elsewhere (the other device).
            self.sync.silent = true;
            self.sync.new_songs = 0;
            self.sync.errors.clear();
        }
        self.sync.running = true;
        self.sync.available = Some(true);
        let ui = self.win.global::<crate::Ui>();
        ui.set_sync_progress_visible(true);
        let label = if e.state == "started" && e.device.is_empty() {
            "Starting sync…".to_string()
        } else if e.state == "error" {
            if e.device.is_empty() {
                format!("Sync failed: {}", e.message)
            } else {
                format!("Sync with {} failed: {}", e.device, e.message)
            }
        } else if e.state == "done" {
            if !e.message.is_empty() {
                e.message.clone()
            } else if e.done > 0 {
                format!("Got {} from {}", plural(e.done, "new song"), e.device)
            } else {
                format!("{} is in sync", e.device)
            }
        } else {
            let who = if e.device.is_empty() { String::new() } else { format!(" with {}", e.device) };
            let count = if e.total > 0 { format!(" · {}/{}", e.done, e.total) } else { String::new() };
            if e.message.is_empty() {
                format!("Syncing{who}{count}")
            } else {
                format!("{}{count}", e.message)
            }
        };
        ui.set_sync_label(label.into());
        ui.set_sync_label_error(e.state == "error");
        ui.set_sync_fraction(if e.total > 0 { (e.done as f32 / e.total as f32).clamp(0.0, 1.0) } else { -1.0 });
        self.update_devices_ui();
        if !e.finished() {
            return;
        }
        if e.state == "error" {
            self.sync.errors.push(if e.device.is_empty() { e.message.clone() } else { format!("{}: {}", e.device, e.message) });
        } else if e.done > 0 {
            self.sync.new_songs += e.done;
        }
        // More devices may follow; ask the backend whether the run is over.
        let t = slint::Timer::default();
        t.start(slint::TimerMode::SingleShot, Duration::from_millis(800), || {
            with_app(|a| {
                a.sync.settle = None;
                let api = a.api.clone();
                a.spawn(async move {
                    let r = api.sync_devices().await;
                    post(move |a| {
                        let done = r.as_ref().map(|s| !s.syncing).unwrap_or(false);
                        a.sync_loaded(r);
                        if done && a.sync.running {
                            a.finish_sync();
                        }
                    });
                });
            })
        });
        self.sync.settle = Some(t);
    }

    fn finish_sync(&mut self) {
        self.sync.running = false;
        self.update_devices_ui();
        if self.sync.new_songs > 0 {
            let n = self.sync.new_songs;
            self.refresh_all();
            self.load_recs();
            self.snack(format!("Synced {n} new {} from your devices", if n == 1 { "song" } else { "songs" }));
        } else if !self.sync.silent {
            let msg = if self.sync.errors.is_empty() {
                "Everything is already in sync".to_string()
            } else {
                format!("Sync failed — {}", self.sync.errors.join("; "))
            };
            self.snack(msg);
        }
    }

    pub fn pair_code_edited(&mut self, text: String) {
        let clean = normalize_pair_code(&text);
        let ui = self.win.global::<crate::Ui>();
        ui.set_pair_code(clean.into());
        ui.set_pair_error(SharedString::new());
    }

    pub fn pair(&mut self) {
        let ui = self.win.global::<crate::Ui>();
        if ui.get_pairing() {
            return;
        }
        let code = normalize_pair_code(&ui.get_pair_code());
        if code.len() != 6 {
            ui.set_pair_error("Type the 6-letter code shown on the other device.".into());
            return;
        }
        ui.set_pairing(true);
        ui.set_pair_error(SharedString::new());
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.pair_device(&code).await;
            post(move |a| {
                let ui = a.win.global::<crate::Ui>();
                ui.set_pairing(false);
                match r {
                    Ok(_) => {
                        ui.set_pair_code(SharedString::new());
                        a.load_sync();
                    }
                    Err(e) => {
                        let msg = if e.offline() {
                            e.message.clone()
                        } else if e.status == 429 {
                            "Too many tries. Wait a minute and try again.".into()
                        } else if e.status == 404 && e.message.to_lowercase() == "not found" {
                            "This backend does not support device sync yet.".into()
                        } else if e.status == 404 {
                            e.message.clone()
                        } else {
                            format!("Could not pair: {}", e.message)
                        };
                        ui.set_pair_error(msg.into());
                    }
                }
            });
        });
    }

    pub fn ask_remove_device(&mut self, id: String) {
        let name = self
            .sync
            .status
            .as_ref()
            .and_then(|s| s.devices.iter().find(|d| d.id == id))
            .map(|d| if d.name.is_empty() { d.address.clone() } else { d.name.clone() })
            .unwrap_or_default();
        self.dev_timer = None;
        let ui = self.win.global::<crate::Ui>();
        ui.set_dlg_title(format!("Remove {name}?").into());
        ui.set_dlg_message("It stops syncing with this device. No songs or playlists are deleted.".into());
        ui.set_dlg_confirm("Remove".into());
        ui.set_dialog("confirm".into());
        self.dialog = DialogCtx::RemoveDevice(id);
    }

    fn remove_device(&mut self, id: String) {
        if let Some(s) = self.sync.status.as_mut() {
            s.devices.retain(|d| d.id != id);
        }
        self.update_devices_ui();
        let api = self.api.clone();
        self.spawn(async move {
            if let Err(e) = api.remove_device(&id).await {
                post(move |a| {
                    a.snack(format!("Could not remove the device: {e}"));
                    a.load_sync();
                });
            }
        });
    }

    pub fn new_code(&mut self) {
        let api = self.api.clone();
        self.spawn(async move {
            let r = api.new_sync_code().await;
            post(move |a| match r {
                Ok(me) => {
                    if let Some(s) = a.sync.status.as_mut() {
                        s.this_device = me;
                    }
                    a.update_devices_ui();
                }
                Err(e) => a.snack(format!("Could not make a new code: {e}")),
            });
        });
    }

    pub fn copy_code(&mut self) {
        let code = self.win.global::<crate::Ui>().get_dev_code().to_string();
        let ok = copy_to_clipboard(&code);
        self.snack(if ok { "Code copied" } else { "Could not copy the code" });
    }

    // ------------------------------------------------------------ keyboard

    pub fn key_pressed(&mut self, text: &str, ctrl: bool, _shift: bool) -> bool {
        use slint::platform::Key;
        let is = |k: Key| text.starts_with(char::from(k)) && text.chars().count() == 1;
        let ui = self.win.global::<crate::Ui>();
        if is(Key::Escape) {
            if ui.get_menu_open() {
                ui.set_menu_open(false);
            } else if !ui.get_dialog().is_empty() {
                self.dialog_cancel();
            } else if self.win.global::<crate::Player>().get_now_playing_open() {
                self.win.global::<crate::Player>().set_now_playing_open(false);
            } else if self.route() == Route::Downloads {
                self.selected.clear();
                self.update_downloads();
            }
            return true;
        }
        if !ui.get_dialog().is_empty() {
            return false;
        }
        if ctrl && text.eq_ignore_ascii_case("f") {
            self.win.global::<crate::Player>().set_now_playing_open(false);
            self.go(Route::Search);
            return true;
        }
        if text == " " && !ctrl {
            self.play_pause();
            return true;
        }
        if ctrl && is(Key::RightArrow) {
            self.next(false);
            return true;
        }
        if ctrl && is(Key::LeftArrow) {
            self.previous();
            return true;
        }
        if self.route() == Route::Downloads && !self.win.global::<crate::Player>().get_now_playing_open() {
            if ctrl && text.eq_ignore_ascii_case("a") {
                self.dl_select_all();
                return true;
            }
            if is(Key::Delete) && !self.selected.is_empty() {
                let ids: Vec<String> = self.selected.drain().collect();
                self.dl_remove(ids, true);
                return true;
            }
        }
        false
    }
}

fn status_of(t: &Track, existing: Option<&HashSet<String>>) -> i32 {
    if existing.map(|e| e.contains(&t.video_id)).unwrap_or(false) {
        0
    } else if t.downloaded {
        1
    } else {
        2
    }
}

/// (in playlist, in library, will download), counting each song once.
fn counts(p: &PlaylistPreview, existing: Option<&HashSet<String>>) -> (i64, i64, i64) {
    let mut seen = HashSet::new();
    let mut c = (0, 0, 0);
    for t in &p.tracks {
        if !seen.insert(t.video_id.clone()) {
            continue;
        }
        match status_of(t, existing) {
            0 => c.0 += 1,
            1 => c.1 += 1,
            _ => c.2 += 1,
        }
    }
    c
}

pub fn normalize_pair_code(input: &str) -> String {
    input.to_uppercase().chars().filter(|c| c.is_ascii_uppercase()).take(6).collect()
}

fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    for (cmd, args) in [("wl-copy", vec![]), ("xclip", vec!["-selection", "clipboard"])] {
        if let Ok(mut child) = std::process::Command::new(cmd).args(&args).stdin(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            return child.wait().map(|s| s.success()).unwrap_or(false);
        }
    }
    false
}

/// Unix seconds from a JSON number (ms) or an RFC 3339 string.
fn parse_time(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_f64() {
        return Some((n / 1000.0) as i64);
    }
    let s = v.as_str()?;
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let mut t = days * 86400 + h * 3600 + mi * 60 + se;
    // offset like +02:00
    let rest = &s[19..];
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = rest.chars().next().filter(|c| *c == '+' || *c == '-') {
        let oh: i64 = rest.get(1..3).and_then(|x| x.parse().ok()).unwrap_or(0);
        let om: i64 = rest.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
        let off = oh * 3600 + om * 60;
        t -= if sign == '+' { off } else { -off };
    }
    Some(t)
}

fn format_ago(t: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let d = now - t;
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{} min ago", d / 60)
    } else if d < 86400 {
        format!("{} hr ago", d / 3600)
    } else if d < 7 * 86400 {
        format!("{} ago", plural(d / 86400, "day"))
    } else {
        let days = t.div_euclid(86400);
        // civil from days
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        format!("{y}-{m:02}-{d:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_codes() {
        assert_eq!(normalize_pair_code("ab-c d1efgh"), "ABCDEF");
    }

    #[test]
    fn times() {
        assert_eq!(parse_time(&serde_json::json!("1970-01-02T00:00:00Z")), Some(86400));
        assert_eq!(parse_time(&serde_json::json!("1970-01-02T02:00:00+02:00")), Some(86400));
        assert_eq!(parse_time(&serde_json::json!(86400000)), Some(86400));
    }
}
