//! Connects the Slint callbacks to the controller.

use slint::ComponentHandle;

use crate::app::{with_app, App, Route, CLOCK, DL};
use crate::{AppWindow, Lib, Nav, Page, Player, Ui};

fn on(f: impl FnOnce(&mut App) + 'static) {
    with_app(f);
}

impl App {
    fn card_clicked(&mut self, key: &str, i: usize) {
        match key {
            "home-playlists" => {
                if let Some(p) = self.playlists.as_ref().and_then(|l| l.get(i)) {
                    let id = p.id;
                    self.push(Route::Playlist(id));
                }
            }
            "playlists" => {
                if i == 0 {
                    self.open_new_playlist(true, None);
                } else if let Some(p) = self.playlists.as_ref().and_then(|l| l.get(i - 1)) {
                    let id = p.id;
                    self.push(Route::Playlist(id));
                }
            }
            "artists" => {
                if let Some(a) = self.artists.get(i) {
                    let n = a.name.clone();
                    self.push(Route::Artist(n));
                }
            }
            "albums" => {
                if let Some(a) = self.albums.get(i) {
                    let (n, ar) = (a.name.clone(), a.artist.clone());
                    self.push(Route::Album(n, ar));
                }
            }
            k => {
                let list = self.list(k);
                self.play_list(list, i, None);
            }
        }
    }

    fn card_play(&mut self, key: &str, i: usize) {
        if key == "home-playlists" || key == "playlists" {
            let idx = if key == "playlists" { i.wrapping_sub(1) } else { i };
            let Some(p) = self.playlists.as_ref().and_then(|l| l.get(idx)) else { return };
            let id = p.id;
            let api = self.api.clone();
            self.spawn(async move {
                if let Ok(p) = api.playlist(id).await {
                    crate::app::post(move |a| a.play_list(p.tracks, 0, None));
                }
            });
            return;
        }
        let list = self.list(key);
        self.play_list(list, i, None);
    }
}

pub fn wire(win: &AppWindow) {
    let nav = win.global::<Nav>();
    nav.on_go(|p| {
        on(move |a| {
            a.go(match p {
                Page::Home => Route::Home,
                Page::Search => Route::Search,
                Page::Library => Route::Library,
                Page::Downloads => Route::Downloads,
                Page::Settings => Route::Settings,
                _ => Route::Home,
            })
        })
    });
    nav.on_pop(|| on(|a| a.pop()));
    nav.on_open_playlist(|id, push| on(move |a| if push { a.push(Route::Playlist(id as i64)) } else { a.go(Route::Playlist(id as i64)) }));
    nav.on_open_artist(|n| on(move |a| a.push(Route::Artist(n.to_string()))));
    nav.on_open_album(|n, ar| on(move |a| a.push(Route::Album(n.to_string(), ar.to_string()))));

    let p = win.global::<Player>();
    p.on_play_pause(|| on(|a| a.play_pause()));
    p.on_next(|| on(|a| a.next(false)));
    p.on_previous(|| on(|a| a.previous()));
    p.on_toggle_shuffle(|| on(|a| a.toggle_shuffle()));
    p.on_cycle_repeat(|| on(|a| a.cycle_repeat()));
    p.on_seek(|f| on(move |a| a.seek_fraction(f)));
    p.on_seek_ms(|ms| on(move |a| a.seek_ms(ms as i64)));
    p.on_set_volume(|v| on(move |a| a.set_volume(v)));
    p.on_toggle_mute(|| on(|a| a.set_volume(if a.volume <= 0.0 { 80.0 } else { 0.0 })));
    p.on_jump_to(|i| on(move |a| a.jump_to(i as i64)));
    p.on_remove_up_next(|i| on(move |a| a.remove_up_next(i as i64)));
    p.on_move_up_next(|f, t| on(move |a| a.move_up_next(f as i64, t as i64)));
    p.on_clear_up_next(|| on(|a| a.clear_up_next()));
    p.on_download_up_next(|| {
        on(|a| {
            let l = a.up_next().to_vec();
            a.download_all(l)
        })
    });
    p.on_add_current_to_playlist(|| on(|a| a.add_current_to_playlist()));
    p.on_wave_path(|end, h, large, _| CLOCK.with(|c| crate::waves::seek_wave(end, h, large, &c.borrow()).into()));

    let lib = win.global::<Lib>();
    lib.on_dl_state(|id, _| DL.with(|m| m.borrow().get(id.as_str()).map(|d| d.0).unwrap_or(0)));
    lib.on_dl_progress(|id, _| DL.with(|m| m.borrow().get(id.as_str()).map(|d| d.1).unwrap_or(0.0)));
    lib.on_play(|l, i| {
        on(move |a| {
            let t = a.list(&l);
            a.play_list(t, i as usize, None)
        })
    });
    lib.on_play_all(|l, shuffle| {
        on(move |a| {
            let t = a.list(&l);
            a.play_list(t, 0, Some(shuffle))
        })
    });
    lib.on_track_clicked(|l, i| on(move |a| a.track_clicked(&l, i as usize)));
    lib.on_download(|l, i| {
        on(move |a| {
            if let Some(t) = a.list(&l).get(i as usize).cloned() {
                a.download(t)
            }
        })
    });
    lib.on_card_clicked(|k, i| on(move |a| a.card_clicked(&k, i as usize)));
    lib.on_card_play(|k, i| on(move |a| a.card_play(&k, i as usize)));
    lib.on_card_download(|k, i| {
        on(move |a| {
            if let Some(t) = a.list(&k).get(i as usize).cloned() {
                a.download(t)
            }
        })
    });
    lib.on_refresh_recs(|| on(|a| a.refresh_recs()));
    lib.on_retry_home(|| on(|a| a.refresh_all()));
    lib.on_set_library_filter(|t| {
        on(move |a| {
            a.win.global::<crate::Lib>().set_library_filter(t);
            a.update_library()
        })
    });
    lib.on_set_search(|t| on(move |a| a.set_search(t.to_string())));
    lib.on_clear_search(|| on(|a| a.set_search(String::new())));
    lib.on_import_preview(|| on(|a| a.import_preview()));
    lib.on_preview_play(|| {
        on(|a| {
            let t = a.search_results.clone();
            a.play_list(t, 0, None)
        })
    });
    lib.on_download_all_playlist(|| {
        on(|a| {
            let t = a.list("playlist");
            a.download_all(t)
        })
    });
    lib.on_new_playlist(|| on(|a| a.open_new_playlist(true, None)));
    lib.on_toggle_private(|| on(|a| a.toggle_private()));
    lib.on_open_devices(|| on(|a| a.open_devices()));
    lib.on_reorder_playlist(|f, t| on(move |a| a.reorder_playlist(f as i64, t as i64)));
    lib.on_dl_toggle(|id, shift| on(move |a| a.dl_toggle(id.to_string(), shift)));
    lib.on_dl_section_toggle(|s| on(move |a| a.dl_section_toggle(s.to_string())));
    lib.on_dl_remove(|id| on(move |a| a.dl_remove(vec![id.to_string()], false)));
    lib.on_dl_retry(|id| on(move |a| a.dl_retry(vec![id.to_string()])));
    lib.on_dl_retry_all(|| {
        on(|a| {
            let ids = a.downloads.iter().filter(|d| d.is_failed()).map(|d| d.id.clone()).collect();
            a.dl_retry(ids)
        })
    });
    lib.on_dl_clear(|s| on(move |a| a.dl_clear(s.to_string())));
    lib.on_dl_clear_selection(|| {
        on(|a| {
            a.selected.clear();
            a.update_downloads()
        })
    });
    lib.on_dl_remove_selected(|| {
        on(|a| {
            let ids: Vec<String> = a.selected.drain().collect();
            a.dl_remove(ids, true)
        })
    });
    lib.on_dl_select_all(|| on(|a| a.dl_select_all()));

    let ui = win.global::<Ui>();
    ui.on_open_track_menu(|l, i, x, y| on(move |a| a.open_track_menu(l.to_string(), i as usize, x, y)));
    ui.on_open_theme_menu(|x, y| on(move |a| a.open_theme_menu(x, y)));
    ui.on_open_sort_menu(|x, y| on(move |a| a.open_sort_menu(x, y)));
    ui.on_open_playlist_menu(|x, y| on(move |a| a.open_playlist_menu(x, y)));
    ui.on_menu_selected(|i| on(move |a| a.menu_selected(i)));
    ui.on_dialog_confirm(|| on(|a| a.dialog_confirm()));
    ui.on_dialog_cancel(|| on(|a| a.dialog_cancel()));
    ui.on_picker_chosen(|i| on(move |a| a.picker_chosen(i)));
    ui.on_np_name_edited(|t| on(move |a| a.np_name_edited(t.to_string())));
    ui.on_import_url_edited(|t| on(move |a| a.import_url_edited(t.to_string())));
    ui.on_paste_url(|| on(|a| a.paste_url()));
    ui.on_pair(|| on(|a| a.pair()));
    ui.on_pair_code_edited(|t| on(move |a| a.pair_code_edited(t.to_string())));
    ui.on_copy_code(|| on(|a| a.copy_code()));
    ui.on_new_code(|| on(|a| a.new_code()));
    ui.on_remove_device(|id| on(move |a| a.ask_remove_device(id.to_string())));
    ui.on_np_opened(|| on(|a| a.np_opened()));
    ui.on_sync_now(|| on(|a| a.run_sync(false)));
    // content panel moved (sidebar collapse, queue): re-cut the baked wave strip now
    win.on_panel_moved(|| with_app(|a| a.panel_moved()));
    ui.on_resized(|| {
        // Re-render the static wave frames at the new size (debounced).
        thread_local!(static T: slint::Timer = slint::Timer::default());
        T.with(|t| t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(120), || with_app(|a| a.render_static())));
    });
    let weak = win.as_weak();
    ui.on_key_pressed(move |text, ctrl, shift| {
        // Must answer synchronously: borrow directly, never defer.
        let _ = &weak;
        crate::app::try_with_app(|a| a.key_pressed(&text, ctrl, shift)).unwrap_or(false)
    });
    let weak = win.as_weak();
    ui.on_minimize(move || {
        if let Some(w) = weak.upgrade() {
            w.window().set_minimized(true);
        }
    });
    ui.on_close(|| {
        let _ = slint::quit_event_loop();
    });
}
