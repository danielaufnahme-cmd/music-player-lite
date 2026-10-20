//! Theme colours and the wave animation driver.

use std::time::{Duration, Instant};

use slint::ComponentHandle;
use tiny_skia::Pixmap;

use crate::app::{post, with_app, App, CLOCK};
use crate::palette::{build_palette, desktop_scheme_path, wave_colors, DesktopScheme, Palette};
use crate::settings::ThemeSource;
use crate::waves::{
    render, render_over, render_waves_cols, render_waves_split, to_image_rows, to_rgb_image_rows, upscale_into, wave_rows, Pass, WaveSpec,
};
use crate::{Lib, Theme};

/// One backdrop (aurora + waves) split into three stacked images so that a
/// wave frame only repaints the thin strip the crests move through:
/// `top` = backdrop above the crests, `mid` = the animated strip,
/// `low` = rows below every crest (static while the energy is steady).
#[derive(Default)]
struct Scene {
    /// Full frame of the backdrop without waves (aurora + base colour),
    /// stretched from `aurora_small`.
    aurora: Option<Pixmap>,
    aurora_small: Option<Pixmap>,
    mid: Option<Pixmap>,
    /// `mid` before the moving part of the waves: backdrop + the rows of
    /// each layer that are the same every frame (rebuilt like `low`).
    mid_base: Option<Pixmap>,
    low: Option<Pixmap>,
    /// Frame size and strip rows `[r0, r1)` in frame pixels.
    size: (u32, u32),
    rows: (u32, u32),
    /// Energy `low` was drawn with (`None` = must redraw).
    low_energy: Option<f32>,
    /// Fractions of the height last sent to the UI.
    fracs: (f32, f32),
}

impl Scene {
    fn clear(&mut self) {
        *self = Scene::default();
    }
}

#[derive(Default)]
pub struct Buffers {
    shell: Scene,
    cut: ShellCut,
    np: Scene,
    bar: Scene,
    pub palette: Option<Palette>,
    aurora_last: Option<Instant>,
    /// (size px, accent) of the cover glow image last sent.
    glow_key: Option<(u32, [u32; 3])>,
    bar_tick: u32,
}

/// Time between aurora steps.
fn aurora_period(np: bool) -> Duration {
    Duration::from_millis(if np { 490 } else { 990 })
}

/// Wave frame rates (software rendering cost scales with frames): the
/// faint shell backdrop and Now Playing; halved while another window has
/// the focus.
pub const SHELL_FPS: u32 = 7;
pub const NP_FPS: u32 = 20;

/// Time between animation frames for these settings, or `None` when nothing
/// animates (then there is no timer at all). Waves off but aurora on: only
/// the slow aurora steps. `max_fps` caps every rate.
pub fn frame_interval(v: crate::settings::Visual, np: bool, focused: bool) -> Option<Duration> {
    let cap = Duration::from_millis(1000 / v.max_fps.max(1) as u64);
    if v.waves_enabled {
        let fps = if np { NP_FPS } else { SHELL_FPS };
        let fps = if focused { fps } else { (fps / 2).max(1) };
        Some(Duration::from_millis(1000 / fps as u64).max(cap))
    } else if v.aurora_enabled {
        Some(aurora_period(np).max(cap))
    } else {
        None
    }
}

/// The aurora is drawn at 1/AURORA_DIV of the frame size and stretched.
const AURORA_DIV: u32 = 4;

fn ensure(pm: &mut Option<Pixmap>, w: u32, h: u32) -> &mut Pixmap {
    let w = w.max(2);
    let h = h.max(2);
    if pm.as_ref().map(|p| p.width() != w || p.height() != h).unwrap_or(true) {
        *pm = Pixmap::new(w, h);
    }
    pm.as_mut().unwrap()
}

/// Copy rows `y0..` of `src` (same width) into `dst`, or clear `dst` when there is no source.
fn copy_rows(dst: &mut Pixmap, src: Option<&Pixmap>, y0: u32) {
    match src {
        Some(src) if src.width() == dst.width() => {
            let row = src.width() as usize * 4;
            let n = dst.data().len();
            let from = y0 as usize * row;
            dst.data_mut().copy_from_slice(&src.data()[from..from + n]);
        }
        _ => dst.fill(tiny_skia::Color::TRANSPARENT),
    }
}

/// Images for one frame of a scene (`None` = unchanged).
struct Frame {
    top: Option<slint::Image>,
    /// The strip changed: build its image(s) from `Scene::mid`.
    mid: bool,
    low: Option<slint::Image>,
    fracs: Option<(f32, f32)>,
    /// Backdrop, size or energy changed: cached strip bases must be rebuilt.
    base_dirty: bool,
}

/// Render what changed in `sc`. `backdrop`: the scene has an opaque aurora
/// backdrop (RGB images) instead of transparent waves (the player bar).
#[allow(clippy::too_many_arguments)]
fn render_scene(
    sc: &mut Scene,
    fw: u32,
    fh: u32,
    k: f32,
    colors: &crate::palette::WaveColors,
    clock: &crate::waves::WaveClock,
    aurora_spec: Option<&WaveSpec>,
    wave_spec: &WaveSpec,
    new_aurora: bool,
    strip: bool,
) -> Frame {
    let (fw, fh) = (fw.max(2), fh.max(2));
    let mut out = Frame { top: None, mid: false, low: None, fracs: None, base_dirty: false };
    let (t, b) = wave_rows(wave_spec, colors.layers.len(), fh as f32, k);
    let rows = ((t.floor() as u32).min(fh), (b.ceil() as u32).min(fh));
    let resized = sc.size != (fw, fh) || sc.rows != rows;
    sc.size = (fw, fh);
    let mut aurora_changed = false;
    if let Some(aspec) = aurora_spec {
        let stale = sc.aurora.as_ref().map(|a| a.width() != fw || a.height() != fh).unwrap_or(true);
        if new_aurora || stale {
            let small = ensure(&mut sc.aurora_small, fw.div_ceil(AURORA_DIV), fh.div_ceil(AURORA_DIV));
            render(small, k / AURORA_DIV as f32, colors, clock, aspec);
            let pm = ensure(&mut sc.aurora, fw, fh);
            upscale_into(sc.aurora_small.as_ref().unwrap(), pm);
            aurora_changed = true;
        }
    } else {
        sc.aurora = None;
        sc.aurora_small = None;
    }
    sc.rows = rows;
    let (r0, r1) = rows;
    let to_img = |pm: &Pixmap, y0: u32, y1: u32| -> slint::Image {
        if aurora_spec.is_some() {
            to_rgb_image_rows(pm, y0, y1)
        } else {
            to_image_rows(pm, y0, y1)
        }
    };
    if aurora_changed || resized {
        out.top = Some(match &sc.aurora {
            Some(a) if r0 > 0 => to_img(a, 0, r0),
            _ => slint::Image::default(),
        });
    }
    // rows below every crest, and the frame-invariant part of the strip:
    // only when the backdrop, size or energy changed
    if aurora_changed || resized || sc.low_energy != Some(clock.energy) {
        out.base_dirty = true;
        if r1 > r0 && strip {
            let pm = ensure(&mut sc.mid_base, fw, r1 - r0);
            copy_rows(pm, sc.aurora.as_ref(), r0);
            render_waves_split(pm, k, colors, clock, &WaveSpec { y_off: r0 as f32, full_h: fh as f32, ..*wave_spec }, Pass::Static);
        }
        if r1 < fh {
            let pm = ensure(&mut sc.low, fw, fh - r1);
            copy_rows(pm, sc.aurora.as_ref(), r1);
            render_over(pm, k, colors, clock, &WaveSpec { y_off: r1 as f32, full_h: fh as f32, ..*wave_spec });
            out.low = Some(to_img(pm, 0, fh - r1));
        } else {
            sc.low = None;
            out.low = Some(slint::Image::default());
        }
        sc.low_energy = Some(clock.energy);
    }
    // the animated strip: every frame (the shell cuts its own, see ShellCut)
    if r1 > r0 && strip {
        let pm = ensure(&mut sc.mid, fw, r1 - r0);
        copy_rows(pm, sc.mid_base.as_ref(), 0);
        render_waves_split(pm, k, colors, clock, &WaveSpec { y_off: r0 as f32, full_h: fh as f32, ..*wave_spec }, Pass::Moving);
    } else {
        sc.mid = None;
    }
    out.mid = true;
    let fracs = (r0 as f32 / fh as f32, r1 as f32 / fh as f32);
    if fracs != sc.fracs {
        sc.fracs = fracs;
        out.fracs = Some(fracs);
    }
    out
}

/// Premultiplied 0..=255 channels of a Slint colour.
fn premul(c: slint::Color) -> [u32; 4] {
    let a = c.alpha() as u32;
    let m = |v: u8| (v as u32 * a + 127) / 255;
    [m(c.red()), m(c.green()), m(c.blue()), a]
}

/// The shell's wave strip, cut in two around the content panel's left edge:
/// `left` (frame columns up to the panel) and `right` (the panel + whatever
/// is right of it) with the panel's tint + gradient baked into its first
/// `n_tint` columns. app.slint then leaves out its gradient layer in the
/// strip rows: one layer less to repaint per wave frame. The right piece has
/// its own column grid that starts exactly at the panel edge and puts a
/// column boundary (within a hundredth of a pixel) on its right edge, so the
/// baked tint lines up with the panel drawn above and below the strip.
#[derive(Default)]
struct ShellCut {
    left: Piece,
    right: Piece,
    layout: Option<CutLayout>,
    /// (panel x, y, w, h, window w, h, frame w, strip rows) the pieces were laid out for
    key: Option<[f32; 10]>,
    sent: Option<(f32, f32, f32)>,
}

#[derive(Default)]
struct Piece {
    pm: Option<Pixmap>,
    base: Option<Pixmap>,
    /// frame x of each column centre
    xmap: Vec<f32>,
}

impl Piece {
    /// Backdrop columns (nearest frame column) + the static wave rows.
    #[allow(clippy::too_many_arguments)]
    fn rebuild(
        &mut self,
        aurora: &Pixmap,
        r0: u32,
        r1: u32,
        k: f32,
        colors: &crate::palette::WaveColors,
        clock: &crate::waves::WaveClock,
        spec: &WaveSpec,
        fw: f32,
    ) {
        let n = self.xmap.len().max(1) as u32;
        let base = ensure(&mut self.base, n, r1 - r0);
        let (aw, bw) = (aurora.width() as usize, base.width() as usize);
        let src = aurora.data();
        let cols: Vec<usize> = self.xmap.iter().map(|&x| (x.floor().max(0.0) as usize).min(aw - 1) * 4).collect();
        let dst = base.data_mut();
        for y in 0..(r1 - r0) as usize {
            let srow = &src[(r0 as usize + y) * aw * 4..(r0 as usize + y + 1) * aw * 4];
            let drow = &mut dst[y * bw * 4..(y + 1) * bw * 4];
            for (d, &c) in drow.as_chunks_mut::<4>().0.iter_mut().zip(&cols) {
                d.copy_from_slice(&srow[c..c + 4]);
            }
        }
        render_waves_cols(base, k, colors, clock, spec, Pass::Static, fw, &self.xmap);
    }

    fn frame(
        &mut self,
        k: f32,
        colors: &crate::palette::WaveColors,
        clock: &crate::waves::WaveClock,
        spec: &WaveSpec,
        fw: f32,
    ) -> Option<&Pixmap> {
        let base = self.base.as_ref()?;
        let pm = ensure(&mut self.pm, base.width(), base.height());
        pm.data_mut().copy_from_slice(base.data());
        render_waves_cols(pm, k, colors, clock, spec, Pass::Moving, fw, &self.xmap);
        self.pm.as_ref()
    }
}

/// How Slint's software renderer stretches an image of `n` columns over `w`
/// physical pixels: pixel X shows column floor(X · d / 256) with
/// d = floor(n · 256 / w) (8-bit fixed point, nearest neighbour). Returns d.
fn slint_step(n: u32, w: u32) -> u32 {
    ((n << 8) / w.max(1)).max(1)
}

/// Frame x (column centre as Slint will show it) of each of `n` columns
/// stretched over `w` physical px starting at logical x `x0`.
fn slint_xmap(n: u32, w: u32, x0: f32, sf: f32, kx: f32) -> Vec<f32> {
    let d = slint_step(n, w);
    (0..n)
        .map(|i| {
            let a = (i << 8).div_ceil(d); // first pixel showing column i
            let b = ((i + 1) << 8).div_ceil(d).min(w).max(a + 1); // first pixel past it
            (x0 + (a + b) as f32 / 2.0 / sf) * kx
        })
        .collect()
}

/// Layout of the shell strip pieces (see [`ShellCut`]).
#[derive(Clone, Debug, PartialEq)]
struct CutLayout {
    left_map: Vec<f32>,
    right_map: Vec<f32>,
    /// columns of the right piece that lie on the panel (get its tint)
    tint: std::ops::Range<usize>,
    /// right piece position and width, logical px
    right_x: f32,
    right_w: f32,
}

/// Lay out the two pieces for the current panel geometry (logical px; `kx`
/// = frame px per logical px). The right piece starts at (or a pixel or two
/// before) the panel's left edge and runs to the window edge; its column
/// count and start are picked so that, with Slint's stepping, the tinted
/// columns begin and end exactly on the panel's edges.
fn shell_cut_layout(px: f32, pw: f32, w: f32, kx: f32, sf: f32) -> CutLayout {
    let px = px.clamp(0.0, w);
    let pw = pw.clamp(1.0, (w - px).max(1.0));
    let left = ((px * kx).ceil() as u32).max(1);
    let left_w = ((left as f32 / kx * sf).round() as u32).max(1);
    let left_map = slint_xmap(left, left_w, 0.0, sf, kx);
    let x0 = (px * sf).round();
    let wp = ((w * sf).round() - x0).max(1.0) as u32; // panel edge .. window edge, physical
    let pp = (((px + pw) * sf).round() - x0).max(1.0) as u32; // panel width, physical
    let q = kx / sf; // columns per physical px
                     // a column boundary exactly at pixel `at` (pixel at-1 and at in different columns)?
    let edge = |at: u32, d: u32| -> Option<usize> { (at == 0 || ((at - 1) * d) >> 8 < (at * d) >> 8).then_some(((at * d) >> 8) as usize) };
    let mut best = None;
    'search: for shift in 0..=4u32.min(x0 as u32) {
        let width = wp + shift;
        let n0 = ((width as f32 * q).round() as u32).max(1);
        for k in 0..=40u32 {
            for n in [n0 + k, n0.saturating_sub(k)] {
                if n == 0 {
                    continue;
                }
                let d = slint_step(n, width);
                if let (Some(t0), Some(t1)) = (edge(shift, d), edge(shift + pp, d)) {
                    best = Some((shift, n, t0..t1));
                    break 'search;
                }
            }
        }
    }
    // (practically never) no exact fit: within a pixel
    let (shift, n, tint) = best.unwrap_or_else(|| {
        let n = ((wp as f32 * q).round() as u32).max(1);
        (0, n, 0..((pp * slint_step(n, wp)) >> 8) as usize)
    });
    let start = (x0 - shift as f32) / sf;
    let right_map = slint_xmap(n, wp + shift, start, sf, kx);
    CutLayout { left_map, right_map, tint, right_x: start, right_w: (wp + shift) as f32 / sf }
}

impl App {
    /// Render and send the shell strip pieces (see [`ShellCut`]).
    #[allow(clippy::too_many_arguments)]
    fn push_shell_strip(
        &mut self,
        base_dirty: bool,
        k: f32,
        colors: &crate::palette::WaveColors,
        clock: &crate::waves::WaveClock,
        spec: &WaveSpec,
        w: f32,
        h: f32,
    ) {
        let win = &self.win;
        let sc = &self.wave_bufs.shell;
        let (fw, fh) = (sc.size.0.max(2) as f32, sc.size.1.max(2) as f32);
        let (r0, r1) = sc.rows;
        let Some(aurora) = sc.aurora.as_ref().filter(|_| r1 > r0) else {
            win.set_shell_baked(false);
            win.set_shell_mid(slint::Image::default());
            win.set_shell_mid_p(slint::Image::default());
            return;
        };
        let (kx, ky) = (fw / w.max(1.0), fh / h.max(1.0));
        let (px, py, pw, ph) = (win.get_panel_x(), win.get_panel_y(), win.get_panel_w(), win.get_panel_h());
        let sf = win.window().scale_factor();
        let key = [px, py, pw, ph, w, h, fw, r0 as f32, r1 as f32, sf];
        let cut = &mut self.wave_bufs.cut;
        if cut.key != Some(key) {
            let l = shell_cut_layout(px, pw, w, kx, sf);
            cut.left.xmap = l.left_map.clone();
            cut.right.xmap = l.right_map.clone();
            cut.layout = Some(l);
            cut.key = Some(key);
            cut.left.base = None;
            cut.right.base = None;
        }
        let spec = WaveSpec { y_off: r0 as f32, full_h: fh, ..*spec };
        for piece in [&mut cut.left, &mut cut.right] {
            if base_dirty || piece.base.is_none() {
                piece.rebuild(aurora, r0, r1, k, colors, clock, &spec, fw);
            }
        }
        // gradient: tint at panel top + 14 px .. end colour at the panel bottom
        let (tint, end) = (premul(win.get_panel_tint()), premul(win.get_panel_end()));
        let (g0, g1) = ((py + 14.0) * ky, (py + ph) * ky);
        let row_tint = move |y: u32| -> [u32; 4] {
            let t = ((r0 as f32 + y as f32 + 0.5 - g0) / (g1 - g0).max(1.0)).clamp(0.0, 1.0);
            let l = |a: u32, b: u32| (a as f32 + (b as f32 - a as f32) * t + 0.5) as u32;
            [l(tint[0], end[0]), l(tint[1], end[1]), l(tint[2], end[2]), l(tint[3], end[3])]
        };
        let Some(layout) = cut.layout.clone() else { return };
        if let Some(pm) = cut.left.frame(k, colors, clock, &spec, fw) {
            win.set_shell_mid(crate::waves::to_rgb_image_cols(pm, 0, pm.width(), None));
        }
        if let Some(pm) = cut.right.frame(k, colors, clock, &spec, fw) {
            win.set_shell_mid_p(crate::waves::to_rgb_image_cols(pm, 0, pm.width(), Some((&row_tint, layout.tint.clone()))));
        }
        let sent = (cut.left.xmap.len() as f32 / fw, layout.right_x, layout.right_w);
        if cut.sent != Some(sent) {
            cut.sent = Some(sent);
            win.set_shell_m1(sent.0);
            win.set_shell_px(sent.1);
            win.set_shell_pw(sent.2);
        }
        if !win.get_shell_baked() {
            win.set_shell_baked(true);
        }
    }

    pub fn following_desktop(&self) -> bool {
        self.settings.theme_source == ThemeSource::Desktop && self.desktop.is_some()
    }

    /// Recompute the palette (cover + desktop scheme + mode) and push it to the UI.
    pub fn apply_theme(&mut self) {
        let desktop = if self.settings.theme_source == ThemeSource::Desktop { self.desktop.as_ref() } else { None };
        let cover = if self.current().is_some() { self.cover_palette } else { None };
        let p = build_palette(cover, desktop, !self.settings.light);
        let t = self.win.global::<Theme>();
        t.set_dark(p.dark);
        t.set_background(p.background.to_slint());
        t.set_surface(p.surface.to_slint());
        t.set_surface_high(p.surface_high.to_slint());
        t.set_outline(p.outline.to_slint());
        t.set_text(p.text.to_slint());
        t.set_muted(p.muted.to_slint());
        t.set_accent(p.accent.to_slint());
        t.set_on_accent(p.on_accent.to_slint());
        t.set_error(if p.dark { crate::palette::Rgba::hex(0xFFB4AB) } else { crate::palette::Rgba::hex(0xBA1A1A) }.to_slint());
        let lib = self.win.global::<Lib>();
        let following = self.following_desktop();
        lib.set_following_desktop(following);
        lib.set_theme_label(
            if following {
                "Desktop colors".to_string()
            } else {
                format!("Default {}", if self.settings.light { "light" } else { "dark" })
            }
            .into(),
        );
        let changed = self.wave_bufs.palette != Some(p);
        self.wave_bufs.palette = Some(p);
        if changed {
            self.render_static();
        }
    }

    /// Re-read the Caelestia scheme when the file changes (polls its mtime
    /// every 2 s; a stat call, no CPU to speak of).
    pub fn watch_desktop_scheme(&mut self) {
        let path = desktop_scheme_path();
        self.spawn(async move {
            let mut last_mtime = None;
            let mut first = true;
            loop {
                let mtime = tokio::fs::metadata(&path).await.ok().and_then(|m| m.modified().ok());
                if first || mtime != last_mtime {
                    // let the shell finish writing the file
                    if !first {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    let scheme = tokio::fs::read_to_string(&path).await.ok().and_then(|s| DesktopScheme::parse(&s));
                    let exists = mtime.is_some();
                    post(move |a| {
                        // A half-written file parses as None; keep the last good scheme.
                        if scheme.is_none() && a.desktop.is_some() && exists {
                            return;
                        }
                        if scheme != a.desktop {
                            a.desktop = scheme;
                            a.apply_theme();
                        }
                    });
                    last_mtime = mtime;
                    first = false;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    fn sizes(&self) -> (f32, f32, f32, f32) {
        let t = self.win.global::<Theme>();
        let (w, h) = (t.get_win_width(), t.get_win_height());
        let queue = self.win.global::<crate::Player>().get_queue_open() && w > 1150.0;
        let collapsed = w < 1120.0 || (queue && w < 1400.0);
        let bar_w = w - if collapsed { 108.0 } else { 280.0 } - if queue { 354.0 } else { 24.0 };
        (w, h, bar_w, self.win.window().scale_factor())
    }

    /// Render the waves for what is on screen and hand the frames to Slint.
    /// `aurora`: also redraw the (slow) aurora layer.
    fn render_frame(&mut self, aurora: bool) {
        let Some(p) = self.wave_bufs.palette else { return };
        let clock = CLOCK.with(|c| *c.borrow());
        let (w, h, bar_w, sf) = self.sizes();
        let np_open = self.win.global::<crate::Player>().get_now_playing_open();
        let colors = wave_colors(p.accent, p.deep, p.dark);
        let none = WaveSpec {
            intensity: 1.0,
            height_factor: 0.42,
            layers: 0,
            aurora: false,
            paint_base: false,
            glow: false,
            vignette: 0.0,
            y_off: 0.0,
            full_h: 0.0,
        };
        let win = &self.win;
        let bufs = &mut self.wave_bufs;
        let mut base_dirty = false;
        if !np_open && bufs.np.size != (0, 0) {
            // Now Playing closed: drop its frame buffers.
            bufs.np.clear();
            bufs.glow_key = None;
            win.set_np_glow(slint::Image::default());
            win.set_np_top(slint::Image::default());
            win.set_np_mid(slint::Image::default());
            win.set_np_low(slint::Image::default());
        }
        let (scene_k, layers, intensity, glow, vignette) = if np_open {
            (sf / 2.0, 4, 1.0, p.dark, if p.dark { 0.5 } else { 0.12 })
        } else {
            (sf / 3.0, 3, if p.dark { 0.38 } else { 0.55 }, false, 0.0)
        };
        let fw = (w * scene_k) as u32;
        let fh = (h * scene_k) as u32;
        let aspec = WaveSpec { aurora: true, paint_base: true, intensity, vignette, ..none };
        let wspec = WaveSpec { layers, intensity, glow, ..none };
        let sc = if np_open { &mut bufs.np } else { &mut bufs.shell };
        let f = render_scene(sc, fw, fh, scene_k, &colors, &clock, Some(&aspec), &wspec, aurora, np_open);
        macro_rules! push {
            ($f:expr, $sc:expr, $rgb:expr, $top:ident, $mid:ident, $low:ident, $f1:ident, $f2:ident) => {{
                let f = $f;
                if let Some((a, b)) = f.fracs {
                    win.$f1(a);
                    win.$f2(b);
                }
                if let Some(i) = f.top {
                    win.$top(i);
                }
                if let Some(i) = f.low {
                    win.$low(i);
                }
                if f.mid {
                    win.$mid(match &$sc.mid {
                        Some(pm) if $rgb => to_rgb_image_rows(pm, 0, pm.height()),
                        Some(pm) => to_image_rows(pm, 0, pm.height()),
                        None => slint::Image::default(),
                    });
                }
            }};
        }
        if np_open {
            push!(f, bufs.np, true, set_np_top, set_np_mid, set_np_low, set_np_f1, set_np_f2);
            // cover glow: size as in nowplaying.slint (cover = clamp(40% h, 200, 500) + 160)
            let size = ((h * 0.40).clamp(200.0, 500.0) + 160.0) * sf;
            let key = (size.round() as u32, [p.accent.r, p.accent.g, p.accent.b].map(|v| (v * 1000.0) as u32));
            if bufs.glow_key != Some(key) {
                bufs.glow_key = Some(key);
                win.set_np_glow(crate::waves::cover_glow(key.0, p.accent));
            }
        } else {
            base_dirty = f.base_dirty;
            let f = Frame { mid: false, ..f };
            push!(f, bufs.shell, true, set_shell_top, set_shell_mid, set_shell_low, set_shell_f1, set_shell_f2);
            // The faint waves inside the player bar move at half the backdrop's
            // frame rate (the bar has many layers to repaint per frame).
            bufs.bar_tick = bufs.bar_tick.wrapping_add(1);
            if aurora || bufs.bar_tick.is_multiple_of(2) {
                let k = sf / 2.0;
                let spec = WaveSpec { intensity: 0.45, height_factor: 0.75, layers: 2, ..none };
                let f = render_scene(
                    &mut bufs.bar,
                    (bar_w.max(10.0) * k) as u32,
                    (88.0 * k) as u32,
                    k,
                    &colors,
                    &clock,
                    None,
                    &spec,
                    false,
                    true,
                );
                push!(f, bufs.bar, false, set_bar_top, set_bar_mid, set_bar_low, set_bar_f1, set_bar_f2);
            }
        }
        if !np_open {
            self.push_shell_strip(base_dirty, scene_k, &colors, &clock, &wspec, w, h);
        }
    }

    /// One frame of everything (colours, size or visual settings changed).
    pub fn render_static(&mut self) {
        self.render_frame(true);
        // Settings may have turned the animation on or off.
        if self.playing {
            self.start_waves();
        }
    }

    /// The content panel moved or resized (sidebar collapse, queue): re-cut
    /// the shell strip around it right away.
    pub fn panel_moved(&mut self) {
        if !self.win.global::<crate::Player>().get_now_playing_open() && self.wave_bufs.shell.size.0 > 0 {
            self.render_frame(false);
        }
    }

    pub fn np_opened(&mut self) {
        self.render_frame(true);
        if self.playing {
            self.start_waves();
        }
    }

    /// Start the wave clock (playback started/stopped: waves ease in/out).
    pub fn start_waves(&mut self) {
        CLOCK.with(|c| c.borrow_mut().idle = 0.0);
        if self.wave_timer.running() {
            return;
        }
        if self.wave_interval().is_none() {
            // Animations off: the static frame stays, no timer at all.
            return;
        }
        self.wave_last = Instant::now();
        self.schedule_wave_tick();
    }

    fn window_focused(&self) -> bool {
        use slint::winit_030::WinitWindowAccessor;
        // None offscreen (no winit window): count as focused.
        self.win.window().with_winit_window(|w| w.has_focus()).unwrap_or(true)
    }

    /// Time between animation frames, or `None` when nothing animates.
    fn wave_interval(&self) -> Option<Duration> {
        let np = self.win.global::<crate::Player>().get_now_playing_open();
        frame_interval(crate::settings::current(), np, self.window_focused())
    }

    fn schedule_wave_tick(&mut self) {
        let Some(interval) = self.wave_interval() else {
            self.win.global::<crate::Player>().set_wave_on(false);
            return;
        };
        self.wave_timer.start(slint::TimerMode::SingleShot, interval, || with_app(|a| a.wave_tick()));
    }

    fn wave_tick(&mut self) {
        let now = Instant::now();
        let dt = (now - self.wave_last).as_secs_f32().min(0.25);
        self.wave_last = now;
        let playing = self.playing;
        let v = crate::settings::current();
        let (ticking, energy) = CLOCK.with(|c| {
            let mut c = c.borrow_mut();
            if v.waves_enabled {
                c.advance(dt, playing);
            } else {
                // waves frozen: only the aurora drifts (its motion follows the phase)
                let e = c.energy;
                c.advance(dt, playing);
                c.energy = e;
            }
            (if v.waves_enabled { c.ticking(playing) } else { playing }, c.energy)
        });
        let visible = !self.win.window().is_minimized();
        if visible {
            // The aurora drifts slowly: ~2 fps on Now Playing, ~1 fps behind
            // the shell (each step repaints the whole window).
            let period = aurora_period(self.win.global::<crate::Player>().get_now_playing_open());
            let due = self.wave_bufs.aurora_last.map(|t| now - t >= period).unwrap_or(true);
            let aurora = v.aurora_enabled && (playing || energy > 0.0) && due;
            if aurora {
                self.wave_bufs.aurora_last = Some(now);
            }
            if v.waves_enabled || aurora {
                self.render_frame(aurora);
            }
            let p = self.win.global::<crate::Player>();
            let on = v.waves_enabled && energy > 0.001;
            if p.get_wave_on() != on {
                p.set_wave_on(on);
            }
            if on {
                p.set_wave_tick(p.get_wave_tick().wrapping_add(1));
            }
        }
        if ticking {
            self.schedule_wave_tick();
        } else {
            // Calm and paused: freeze (no timer, no redraws).
            self.win.global::<crate::Player>().set_wave_on(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Visual;

    fn vis(waves: bool, aurora: bool, max_fps: u32) -> Visual {
        Visual { waves_enabled: waves, aurora_enabled: aurora, max_fps, ..Visual::default() }
    }

    #[test]
    fn settings_gate_the_animation_timer() {
        // everything off (or reduce motion): no timer at all
        assert_eq!(frame_interval(vis(false, false, 30), false, true), None);
        assert_eq!(frame_interval(vis(false, false, 30), true, true), None);
        // waves on: the per-view rates
        assert_eq!(frame_interval(vis(true, true, 30), false, true), Some(Duration::from_millis(1000 / SHELL_FPS as u64)));
        assert_eq!(frame_interval(vis(true, true, 30), true, true), Some(Duration::from_millis(1000 / NP_FPS as u64)));
        // waves off, aurora on: only the slow aurora steps, never wave rate
        let a = frame_interval(vis(false, true, 30), true, true).unwrap();
        assert!(a >= Duration::from_millis(400));
        // max_fps caps every timer
        for np in [false, true] {
            for (w, au) in [(true, true), (true, false), (false, true)] {
                let i = frame_interval(vis(w, au, 12), np, true).unwrap();
                assert!(i >= Duration::from_millis(1000 / 12), "{i:?}");
                let i = frame_interval(vis(w, au, 1), np, true).unwrap();
                assert!(i >= Duration::from_millis(1000), "{i:?}");
            }
        }
        // unfocused window: half the rate
        assert!(frame_interval(vis(true, true, 30), true, false).unwrap() > frame_interval(vis(true, true, 30), true, true).unwrap());
    }

    #[test]
    fn reduce_motion_means_no_timer() {
        let s = crate::settings::Settings { reduce_motion: true, ..Default::default() };
        assert_eq!(frame_interval(s.visual(), false, true), None);
        assert_eq!(frame_interval(s.visual(), true, true), None);
    }
}

#[cfg(test)]
mod cut_tests {
    use super::*;

    /// The tinted columns end exactly at the panel's right edge as Slint
    /// draws them, for many window / panel sizes.
    #[test]
    fn baked_tint_lines_up_with_the_panel() {
        for &(px, pw, w, sf) in &[
            (280.0f32, 1408.0f32, 1700.0, 1.0),
            (108.0, 1240.0, 1372.0, 1.0),
            (280.0, 1066.0, 1700.0, 1.0),
            (280.0, 1409.0, 1701.0, 1.0),
            (280.0, 1408.0, 1700.0, 1.25),
            (108.0, 640.0, 760.0, 2.0),
        ] {
            let fw: f32 = (w * sf / 3.0f32).floor();
            let kx = fw / w;
            let l = shell_cut_layout(px, pw, w, kx, sf);
            let x = (l.right_x * sf).round() as u32;
            let width = (l.right_w * sf).round() as u32;
            let d = slint_step(l.right_map.len() as u32, width);
            let col = |px_abs: u32| (((px_abs - x) * d) >> 8) as usize;
            let (p0, p1) = ((px * sf).round() as u32, ((px + pw) * sf).round() as u32);
            // first and last panel pixel tinted, their neighbours outside not
            assert!(l.tint.contains(&col(p0)) && l.tint.contains(&col(p1 - 1)), "px {px} pw {pw} w {w} sf {sf}");
            assert!(col(p1) >= l.tint.end && (p0 == x || col(p0 - 1) < l.tint.start), "px {px} pw {pw} w {w} sf {sf}");
            let (right, wp) = (&l.right_map, width);
            // columns are about one frame pixel wide
            assert!((right.len() as f32 - wp as f32 * kx / sf).abs() <= 41.0);
        }
    }
}
