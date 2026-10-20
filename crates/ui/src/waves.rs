//! The animated wave/aurora look (port of app/lib/widgets/waves.dart),
//! rendered with tiny-skia into small pixel buffers that Slint stretches.
//! Frames are only produced while music plays (or while the waves calm
//! down after a pause); otherwise the last frame stays on screen.

use std::f32::consts::PI;

use tiny_skia::{Color, GradientStop, Paint, Pixmap, Point, RadialGradient, Rect, SpreadMode, Transform};
#[cfg(test)]
use tiny_skia::{FillRule, LinearGradient, PathBuilder, Stroke};

use crate::palette::{Rgba, WaveColors};

/// Shared animation clock (WaveClock): `energy` eases toward 1 while
/// playing and toward 0 when paused; `phase` advances faster while playing.
#[derive(Clone, Copy, Debug, Default)]
pub struct WaveClock {
    pub phase: f32,
    pub energy: f32,
    pub idle: f32,
}

impl WaveClock {
    /// Seconds after a pause (once calm) before frames stop.
    pub const IDLE_STOP: f32 = 1.0;

    pub fn advance(&mut self, dt: f32, playing: bool) {
        let target = if playing { 1.0 } else { 0.0 };
        self.energy += (target - self.energy) * (1.0 - (-dt * 2.2).exp());
        // Snap when the remaining motion is far below a pixel, so a pause
        // reaches 0 % CPU in ~2.5 s.
        if (self.energy - target).abs() < 0.005 {
            self.energy = target;
        }
        self.phase += dt * (0.12 + 0.88 * self.energy);
        if playing {
            self.idle = 0.0;
        } else {
            self.idle += dt;
        }
    }

    /// True while frames are still needed.
    pub fn ticking(&self, playing: bool) -> bool {
        playing || self.energy > 0.0 || self.idle < Self::IDLE_STOP
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WaveSpec {
    pub intensity: f32,
    pub height_factor: f32,
    pub layers: usize,
    pub aurora: bool,
    pub paint_base: bool,
    pub glow: bool,
    pub vignette: f32,
    /// Render only the waves into a pixmap that shows the band of a taller
    /// frame starting `y_off` px down; `full_h` is that frame's height (0 = pixmap height).
    pub y_off: f32,
    pub full_h: f32,
}

fn col(c: Rgba, a: f32) -> Color {
    c.alpha(a).to_skia()
}

/// Render one frame into `pm` (cleared first). `px` = pixels per logical px.
pub fn render(pm: &mut Pixmap, px: f32, colors: &WaveColors, clock: &WaveClock, spec: &WaveSpec) {
    pm.fill(Color::TRANSPARENT);
    render_over(pm, px, colors, clock, spec);
}

/// Like [`render`] but draws over the current pixmap content.
pub fn render_over(pm: &mut Pixmap, px: f32, colors: &WaveColors, clock: &WaveClock, spec: &WaveSpec) {
    let w = pm.width() as f32;
    let h = pm.height() as f32;
    if w < 2.0 || h < 2.0 {
        return;
    }
    let t = clock.phase;
    let rect = Rect::from_xywh(0.0, 0.0, w, h).unwrap();
    if spec.aurora {
        if spec.paint_base {
            let mut p = Paint::default();
            p.set_color(colors.base.to_skia());
            pm.fill_rect(rect, &p, Transform::identity(), None);
        }
        let n = colors.aurora.len();
        let r = w.max(h);
        for (i, c) in colors.aurora.iter().enumerate() {
            let fi = i as f32;
            let cx = w * (0.18 + 0.64 * (fi / (n.max(2) - 1) as f32)) + w * 0.10 * (t * 0.21 * (fi + 1.0) + fi).sin();
            let cy = h * (0.18 + 0.22 * fi) + h * 0.08 * (t * 0.17 * (fi + 1.0) + fi * 2.0).cos();
            let a = (c.a * spec.intensity).clamp(0.0, 1.0);
            if let Some(shader) = RadialGradient::new(
                Point::from_xy(cx, cy),
                Point::from_xy(cx, cy),
                r * (0.62 - fi * 0.1),
                vec![GradientStop::new(0.0, col(*c, a)), GradientStop::new(1.0, col(*c, 0.0))],
                SpreadMode::Pad,
                Transform::identity(),
            ) {
                let p = Paint { shader, anti_alias: false, ..Default::default() };
                pm.fill_rect(rect, &p, Transform::identity(), None);
            }
        }
        if spec.vignette > 0.0 {
            if let Some(shader) = RadialGradient::new(
                Point::from_xy(w / 2.0, h / 2.0),
                Point::from_xy(w / 2.0, h / 2.0),
                r * 0.78,
                vec![
                    GradientStop::new(0.55, Color::from_rgba8(0, 0, 0, 0)),
                    GradientStop::new(1.0, Color::from_rgba(0.0, 0.0, 0.0, spec.vignette).unwrap()),
                ],
                SpreadMode::Pad,
                Transform::identity(),
            ) {
                let p = Paint { shader, anti_alias: false, ..Default::default() };
                pm.fill_rect(rect, &p, Transform::identity(), None);
            }
        }
    }

    draw_waves(pm, px, colors, clock, spec, Pass::All);
}

/// Which rows of each wave layer to draw (see [`render_waves_split`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pass {
    All,
    /// Only the rows every frame shares: where this layer and all layers
    /// before it cover the row completely (its crest and glow never get there).
    Static,
    /// Everything else: drawn over a `Static` pass gives the `All` picture.
    Moving,
}

/// [`render_waves_split`] for a horizontal slice of a frame `full_w` wide;
/// column i of `pm` shows frame x = `xmap[i]` (column centre).
#[allow(clippy::too_many_arguments)]
pub fn render_waves_cols(
    pm: &mut Pixmap,
    px: f32,
    colors: &WaveColors,
    clock: &WaveClock,
    spec: &WaveSpec,
    pass: Pass,
    full_w: f32,
    xmap: &[f32],
) {
    if pm.width() < 1 || pm.height() < 1 || xmap.len() != pm.width() as usize {
        return;
    }
    draw_waves_cols(pm, px, colors, clock, spec, pass, full_w, Some(xmap));
}

/// Waves only (no aurora), split into a pass that can be cached while the
/// energy stays the same and the per-frame remainder.
pub fn render_waves_split(pm: &mut Pixmap, px: f32, colors: &WaveColors, clock: &WaveClock, spec: &WaveSpec, pass: Pass) {
    if pm.width() < 2 || pm.height() < 2 {
        return;
    }
    draw_waves(pm, px, colors, clock, spec, pass);
}

/// Wave geometry shared by both rasterisers: per layer, the crest polyline
/// (`step` apart) and the fill gradient.
struct Layer {
    color: Rgba,
    a: f32,
    base: f32,
    amp: f32,
    k: f32,
    speed: f32,
    phi: f32,
}

fn layers(colors: &WaveColors, clock: &WaveClock, spec: &WaveSpec, h: f32) -> Vec<Layer> {
    let layers = &colors.layers[..spec.layers.min(colors.layers.len())];
    let n = layers.len();
    if n == 0 || spec.intensity <= 0.0 {
        return Vec::new();
    }
    let _ = h;
    let e = clock.energy;
    let band = h * spec.height_factor;
    layers
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let fi = i as f32;
            let depth = if n == 1 { 1.0 } else { fi / (n - 1) as f32 };
            Layer {
                color: *c,
                a: (c.a * spec.intensity).clamp(0.0, 1.0),
                base: h - band * (1.0 - depth * 0.55),
                amp: band * (0.06 + 0.10 * e) * (1.0 - depth * 0.3) * (0.55 + 0.45 * spec.intensity),
                k: 0.0,
                speed: (0.55 + fi * 0.22) * if i % 2 == 0 { 1.0 } else { -1.0 },
                phi: fi * 1.9,
            }
        })
        .collect()
}

impl Layer {
    fn y_at(&self, x: f32, t: f32) -> f32 {
        self.base
            + self.amp
                * (0.65 * (self.k * x + t * self.speed * 2.0 + self.phi).sin()
                    + 0.35 * (self.k * 2.3 * x - t * self.speed * 1.4 + self.phi * 2.1).sin())
    }
}

/// A premultiplied colour (0..=255 per channel) as two 0x00XX00XX lanes: (r|b<<16, g|a<<16).
#[inline(always)]
fn pack(c: [u32; 4]) -> (u32, u32) {
    (c[0] | c[2] << 16, c[1] | c[3] << 16)
}

/// Source-over of a packed premultiplied colour at coverage `cov`
/// (0..=256) onto one RGBA8 premultiplied pixel (as a little-endian u32),
/// two channels per multiply.
#[inline(always)]
fn over(px: u32, src: (u32, u32), cov: u32) -> u32 {
    const M: u32 = 0x00FF_00FF;
    let (rb, ga) = if cov >= 256 { src } else { (((src.0 * cov) >> 8) & M, ((src.1 * cov) >> 8) & M) };
    let inv = 255 - (ga >> 16);
    let drb = (px & M) * inv;
    let dga = ((px >> 8) & M) * inv;
    // x / 255 for both lanes, rounded
    let drb = ((drb + 0x0080_0080 + ((drb >> 8) & M)) >> 8) & M;
    let dga = ((dga + 0x0080_0080 + ((dga >> 8) & M)) >> 8) & M;
    // add, then saturate a lane that rounded up to 256
    let sat = |v: u32| (v | (((v >> 8) & 0x0001_0001) * 0xFF)) & M;
    sat(rb + drb) | (sat(ga + dga) << 8)
}

fn premul(c: Rgba, a: f32) -> [u32; 4] {
    let a = a.clamp(0.0, 1.0);
    let q = |v: f32| (v.clamp(0.0, 1.0) * a * 255.0 + 0.5) as u32;
    [q(c.r), q(c.g), q(c.b), (a * 255.0 + 0.5) as u32]
}

/// Layered flowing sine waves in the bottom `height_factor` of the box.
///
/// A small dedicated scanline rasteriser (instead of tiny-skia paths +
/// gradient shaders): every layer is "everything below a crest line", with a
/// vertical gradient that depends only on y, so each column is one run of
/// pre-computed row colours with an anti-aliased crest pixel. The glow
/// strokes use the perpendicular distance to the crest segment. Matches the
/// tiny-skia version (see the test) at a fraction of the CPU.
fn draw_waves(pm: &mut Pixmap, px: f32, colors: &WaveColors, clock: &WaveClock, spec: &WaveSpec, pass: Pass) {
    let w = pm.width() as f32;
    draw_waves_cols(pm, px, colors, clock, spec, pass, w, None)
}

/// `draw_waves` for a pixmap whose columns are an arbitrary horizontal slice
/// of a frame `full_w` wide: column i shows frame x = `xmap[i]` (its centre).
#[allow(clippy::too_many_arguments)]
fn draw_waves_cols(
    pm: &mut Pixmap,
    px: f32,
    colors: &WaveColors,
    clock: &WaveClock,
    spec: &WaveSpec,
    pass: Pass,
    full_w: f32,
    xmap: Option<&[f32]>,
) {
    let w = full_w;
    let rows = pm.height() as usize;
    let cols = pm.width() as usize;
    let h = if spec.full_h > 0.0 { spec.full_h } else { pm.height() as f32 };
    let mut ls = layers(colors, clock, spec, h);
    if ls.is_empty() {
        return;
    }
    let nl = ls.len();
    let t = clock.phase;
    let y_off = spec.y_off;
    let step = (w / 140.0).max(4.0 * px);
    let npts = ((w + step) / step).floor() as usize + 2;
    let mut pts = vec![0f32; npts];
    // per layer: crest y and 1/sqrt(1+slope²) per column, packed fill colour per row
    let mut crest = vec![0f32; cols * nl];
    let mut norm = vec![0f32; cols * nl];
    let mut row_col = vec![(0u32, 0u32); rows * nl];
    let mut glow = Vec::with_capacity(nl);
    for (i, l) in ls.iter_mut().enumerate() {
        l.k = 2.0 * PI / w * (0.9 + i as f32 * 0.42);
        // crest polyline, like the path version: x = 0, step, 2·step, ...
        for (j, p) in pts.iter_mut().enumerate() {
            *p = l.y_at(j as f32 * step, t) - y_off;
        }
        for x in 0..cols {
            let fx = xmap.map(|m| m[x]).unwrap_or(x as f32 + 0.5).max(0.0) / step;
            let j = (fx.floor() as usize).min(npts - 2);
            let f = fx - j as f32;
            let c = pts[j] + (pts[j + 1] - pts[j]) * f;
            let sl = (pts[j + 1] - pts[j]) / step;
            crest[i * cols + x] = c;
            norm[i * cols + x] = 1.0 / (1.0 + sl * sl).sqrt();
        }
        // fill gradient: alpha a at (base - amp) .. a·0.12 at the frame bottom
        let (g0, g1) = (l.base - l.amp, h);
        for y in 0..rows {
            let yy = y as f32 + 0.5 + y_off;
            let tt = ((yy - g0) / (g1 - g0)).clamp(0.0, 1.0);
            row_col[i * rows + y] = pack(premul(l.color, l.a * (1.0 - 0.88 * tt)));
        }
        let ca = (l.a * 2.2).clamp(0.0, 0.8);
        glow.push([
            (9.0 * px, pack(premul(l.color, ca * 0.14))),
            (4.5 * px, pack(premul(l.color, ca * 0.28))),
            (1.6 * px, pack(premul(l.color, ca))),
        ]);
    }
    let reach = 4.5 * px + 1.5;
    // first row (local) from which layers 0..=i always cover fully and no glow reaches
    let mut fixed_from = Vec::with_capacity(nl);
    let mut acc = 0usize;
    for l in &ls {
        let lowest = l.base + l.amp - y_off + if spec.glow { reach + 0.5 } else { 0.0 };
        acc = acc.max(lowest.ceil().max(0.0) as usize + 1);
        fixed_from.push(acc.min(rows));
    }
    let data = pm.data_mut();
    for i in 0..nl {
        let (lim_lo, lim_hi) = match pass {
            Pass::All => (0, rows),
            Pass::Static => (fixed_from[i], rows),
            Pass::Moving => (0, fixed_from[i]),
        };
        if pass == Pass::Static {
            for y in lim_lo..lim_hi {
                let src = row_col[i * rows + y];
                for d in data[y * cols * 4..(y + 1) * cols * 4].as_chunks_mut::<4>().0 {
                    let p = over(u32::from_le_bytes([d[0], d[1], d[2], d[3]]), src, 256);
                    d.copy_from_slice(&p.to_le_bytes());
                }
            }
            continue;
        }
        let crest = &crest[i * cols..(i + 1) * cols];
        let norm = &norm[i * cols..(i + 1) * cols];
        let row_col = &row_col[i * rows..(i + 1) * rows];
        let (lo, hi) = crest.iter().fold((f32::MAX, f32::MIN), |(a, b), &c| (a.min(c), b.max(c)));
        // Row by row (sequential memory): rows above the crest are skipped,
        // rows below all of it are a plain run of one colour.
        let y_first = lo.floor().max(0.0) as usize;
        let y_full = (hi.ceil().max(0.0) as usize).min(rows);
        for y in y_first..lim_hi {
            let line = &mut data[y * cols * 4..(y + 1) * cols * 4];
            let src = row_col[y];
            if y >= y_full {
                for d in line.as_chunks_mut::<4>().0 {
                    let p = over(u32::from_le_bytes([d[0], d[1], d[2], d[3]]), src, 256);
                    d.copy_from_slice(&p.to_le_bytes());
                }
                continue;
            }
            let fy = y as f32 + 1.0;
            for (d, &yc) in line.as_chunks_mut::<4>().0.iter_mut().zip(crest) {
                let cov = fy - yc;
                if cov <= 0.0 {
                    continue;
                }
                let p = over(u32::from_le_bytes([d[0], d[1], d[2], d[3]]), src, if cov >= 1.0 { 256 } else { (cov * 256.0) as u32 });
                d.copy_from_slice(&p.to_le_bytes());
            }
        }
        if spec.glow {
            // The three strokes composed into one colour per distance from
            // the crest (source-over is associative): a small lookup table.
            const Q: f32 = 32.0;
            let lut: Vec<(u32, u32)> = (0..=(reach * Q) as usize + 1)
                .map(|k| {
                    let dist = k as f32 / Q;
                    let mut p = 0u32;
                    for (sw, c) in &glow[i] {
                        let cv = (sw / 2.0 + 0.5 - dist).clamp(0.0, 1.0).min(*sw);
                        if cv > 0.0 {
                            p = over(p, *c, (cv * 256.0) as u32);
                        }
                    }
                    (p & 0x00FF_00FF, (p >> 8) & 0x00FF_00FF)
                })
                .collect();
            // column by column: only the few rows around the crest
            for (x, (&yc, &nm)) in crest.iter().zip(norm).enumerate() {
                let y0 = (yc - reach + 0.5).floor().max(0.0) as usize;
                let y1 = ((yc + reach + 0.5).ceil().max(0.0) as usize).min(lim_hi);
                for y in y0..y1 {
                    let dist = ((y as f32 + 0.5 - yc) * nm).abs();
                    if dist >= reach {
                        continue;
                    }
                    let c = lut[(dist * Q + 0.5) as usize];
                    if c.1 == 0 {
                        continue;
                    }
                    let o = (y * cols + x) * 4;
                    let d = &mut data[o..o + 4];
                    let p = over(u32::from_le_bytes([d[0], d[1], d[2], d[3]]), c, 256);
                    d.copy_from_slice(&p.to_le_bytes());
                }
            }
        }
    }
}

/// The original tiny-skia path rasteriser, kept as the reference the fast
/// one is tested against.
#[cfg(test)]
fn draw_waves_skia(pm: &mut Pixmap, px: f32, colors: &WaveColors, clock: &WaveClock, spec: &WaveSpec) {
    let w = pm.width() as f32;
    let h = pm.height() as f32;
    let t = clock.phase;
    let layers = &colors.layers[..spec.layers.min(colors.layers.len())];
    let n = layers.len();
    if n == 0 || spec.intensity <= 0.0 {
        return;
    }
    let e = clock.energy;
    let h = if spec.full_h > 0.0 { spec.full_h } else { h };
    let tf = Transform::from_translate(0.0, -spec.y_off);
    let band = h * spec.height_factor;
    let step = (w / 140.0).max(4.0 * px);
    for (i, c) in layers.iter().enumerate() {
        let fi = i as f32;
        let depth = if n == 1 { 1.0 } else { fi / (n - 1) as f32 };
        let base = h - band * (1.0 - depth * 0.55);
        let amp = band * (0.06 + 0.10 * e) * (1.0 - depth * 0.3) * (0.55 + 0.45 * spec.intensity);
        let k = 2.0 * PI / w * (0.9 + fi * 0.42);
        let speed = (0.55 + fi * 0.22) * if i % 2 == 0 { 1.0 } else { -1.0 };
        let phi = fi * 1.9;
        let y_at =
            |x: f32| base + amp * (0.65 * (k * x + t * speed * 2.0 + phi).sin() + 0.35 * (k * 2.3 * x - t * speed * 1.4 + phi * 2.1).sin());

        let mut crest = PathBuilder::new();
        crest.move_to(0.0, y_at(0.0));
        let mut x = step;
        while x <= w + step {
            crest.line_to(x, y_at(x));
            x += step;
        }
        let crest_path = crest.clone().finish();
        crest.line_to(w + step, h);
        crest.line_to(0.0, h);
        crest.close();
        let Some(fill) = crest.finish() else { continue };
        let a = (c.a * spec.intensity).clamp(0.0, 1.0);
        if let Some(shader) = LinearGradient::new(
            Point::from_xy(0.0, base - amp),
            Point::from_xy(0.0, h),
            vec![GradientStop::new(0.0, col(*c, a)), GradientStop::new(1.0, col(*c, a * 0.12))],
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            let p = Paint { shader, anti_alias: true, ..Default::default() };
            pm.fill_path(&fill, &p, FillRule::Winding, tf, None);
        }
        if spec.glow {
            if let Some(cp) = crest_path {
                let ca = (a * 2.2).clamp(0.0, 0.8);
                for (width, alpha) in [(9.0, ca * 0.14), (4.5, ca * 0.28), (1.6, ca)] {
                    let mut p = Paint::default();
                    p.set_color(col(*c, alpha));
                    p.anti_alias = true;
                    let s = Stroke { width: width * px, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
                    pm.stroke_path(&cp, &p, &s, tf, None);
                }
            }
        }
    }
}

/// The glow behind the Now Playing cover, `size`×`size` physical px: same as
/// Slint's `@radial-gradient(circle, c 26% α 0%, 10% α 45%, transparent 70%)`
/// on a square (radius = half the diagonal).
pub fn cover_glow(size: u32, c: Rgba) -> slint::Image {
    let size = size.max(2);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size, size);
    let half = size as f32 / 2.0;
    let radius = half * std::f32::consts::SQRT_2;
    let stops = [(0.0f32, 0.26f32), (0.45, 0.10), (0.70, 0.0)];
    let (r, g, b) = (c.r.clamp(0.0, 1.0), c.g.clamp(0.0, 1.0), c.b.clamp(0.0, 1.0));
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32 + 0.5 - half, (i as u32 / size) as f32 + 0.5 - half);
        let t = (x * x + y * y).sqrt() / radius;
        let a = if t >= stops[2].0 {
            0.0
        } else if t >= stops[1].0 {
            stops[1].1 + (stops[2].1 - stops[1].1) * (t - stops[1].0) / (stops[2].0 - stops[1].0)
        } else {
            stops[0].1 + (stops[1].1 - stops[0].1) * t / stops[1].0
        };
        let q = |v: f32| (v * a * 255.0 + 0.5) as u8;
        *px = slint::Rgba8Pixel { r: q(r), g: q(g), b: q(b), a: (a * 255.0 + 0.5) as u8 };
    }
    slint::Image::from_rgba8_premultiplied(buf)
}

/// Bilinear upscale of `src` to fill `dst` (premultiplied RGBA). The aurora
/// is only soft radial glows, so it is drawn at a fraction of the size and
/// stretched: same picture, a fraction of the gradient work.
pub fn upscale_into(src: &Pixmap, dst: &mut Pixmap) {
    const M: u32 = 0x00FF_00FF;
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (dw, dh) = (dst.width() as usize, dst.height() as usize);
    let s = src.data();
    let px = |i: usize| u32::from_le_bytes([s[i], s[i + 1], s[i + 2], s[i + 3]]);
    // lerp of two packed pixels, weight 0..=256 of `b`
    let lerp = |a: u32, b: u32, w: u32| -> u32 {
        let rb = ((a & M) * (256 - w) + (b & M) * w) >> 8;
        let ga = (((a >> 8) & M) * (256 - w) + ((b >> 8) & M) * w) >> 8;
        (rb & M) | ((ga & M) << 8)
    };
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    let cols: Vec<(usize, usize, u32)> = (0..dw)
        .map(|x| {
            let sx = ((x as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = sx.floor() as usize;
            (x0 * 4, (x0 + 1).min(sw - 1) * 4, ((sx - x0 as f32) * 256.0) as u32)
        })
        .collect();
    // every source row stretched horizontally once
    let wide: Vec<u32> = (0..sh)
        .flat_map(|y| {
            let base = y * sw * 4;
            cols.iter().map(move |&(a, b, w)| (base + a, base + b, w))
        })
        .map(|(a, b, w)| lerp(px(a), px(b), w))
        .collect();
    let d = dst.data_mut();
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = sy.floor() as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = ((sy - y0 as f32) * 256.0) as u32;
        let r0 = &wide[y0 * dw..(y0 + 1) * dw];
        let r1 = &wide[y1 * dw..(y1 + 1) * dw];
        for ((o, &a), &b) in d[y * dw * 4..(y + 1) * dw * 4].as_chunks_mut::<4>().0.iter_mut().zip(r0).zip(r1) {
            o.copy_from_slice(&lerp(a, b, wy).to_le_bytes());
        }
    }
}

/// Rows (in full-frame pixels, `full_h` tall) that the wave crests can ever
/// touch with this spec, at any phase and energy: `(top, bottom)`, padded for
/// anti-aliasing and the glow strokes. Above `top` the frame shows only the
/// backdrop; below `bottom` every layer covers the pixel with a fill whose
/// gradient depends only on y and energy, so those rows do not change while
/// the waves move (only when energy or the backdrop changes).
pub fn wave_rows(spec: &WaveSpec, n_layers: usize, full_h: f32, px: f32) -> (f32, f32) {
    let n = n_layers.min(spec.layers);
    if n == 0 || spec.intensity <= 0.0 {
        return (full_h, full_h);
    }
    let band = full_h * spec.height_factor;
    let (mut top, mut bottom) = (f32::MAX, f32::MIN);
    for i in 0..n {
        let depth = if n == 1 { 1.0 } else { i as f32 / (n - 1) as f32 };
        let base = full_h - band * (1.0 - depth * 0.55);
        // amplitude at full energy (the largest it gets)
        let amp = band * 0.16 * (1.0 - depth * 0.3) * (0.55 + 0.45 * spec.intensity);
        top = top.min(base - amp);
        bottom = bottom.max(base + amp);
    }
    let pad = if spec.glow { 4.5 * px + 1.0 } else { 1.0 };
    ((top - pad).max(0.0), (bottom + pad).min(full_h))
}

/// Rows `y0..y1` of an opaque pixmap as an RGB Slint image (the software
/// renderer copies opaque RGB pixels without blending: much cheaper per frame).
pub fn to_rgb_image_rows(pm: &Pixmap, y0: u32, y1: u32) -> slint::Image {
    let y1 = y1.min(pm.height()).max(y0 + 1);
    let w = pm.width() as usize;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(pm.width(), y1 - y0);
    let src = &pm.data()[y0 as usize * w * 4..y1 as usize * w * 4];
    for (d, s) in buf.make_mut_bytes().as_chunks_mut::<3>().0.iter_mut().zip(src.chunks_exact(4)) {
        d.copy_from_slice(&s[..3]);
    }
    slint::Image::from_rgb8(buf)
}

/// Columns `x0..x1` of an opaque pixmap as an RGB Slint image; when `tint`
/// is given, each row's columns in its range get that premultiplied colour
/// (0..=255 per channel, alpha last) blended over them — a translucent
/// panel baked in.
pub fn to_rgb_image_cols(pm: &Pixmap, x0: u32, x1: u32, tint: Option<(&dyn Fn(u32) -> [u32; 4], std::ops::Range<usize>)>) -> slint::Image {
    let x1 = x1.min(pm.width()).max(x0 + 1);
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let cw = (x1 - x0) as usize;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(x1 - x0, h as u32);
    let out = buf.make_mut_bytes();
    let data = pm.data();
    for y in 0..h {
        let src = &data[(y * w + x0 as usize) * 4..(y * w + x1 as usize) * 4];
        let dst = &mut out[y * cw * 3..(y + 1) * cw * 3];
        match tint {
            None => {
                for (d, s) in dst.as_chunks_mut::<3>().0.iter_mut().zip(src.chunks_exact(4)) {
                    d.copy_from_slice(&s[..3]);
                }
            }
            Some((f, ref range)) => {
                // the columns in `range` get the tint, the rest are copied
                let c = f(y as u32);
                let inv = 255 - c[3];
                for (k, (d, s)) in dst.as_chunks_mut::<3>().0.iter_mut().zip(src.chunks_exact(4)).enumerate() {
                    if range.contains(&k) {
                        for i in 0..3 {
                            d[i] = (c[i] + (s[i] as u32 * inv + 127) / 255).min(255) as u8;
                        }
                    } else {
                        d.copy_from_slice(&s[..3]);
                    }
                }
            }
        }
    }
    slint::Image::from_rgb8(buf)
}

/// Rows `y0..y1` of a pixmap as a Slint image.
pub fn to_image_rows(pm: &Pixmap, y0: u32, y1: u32) -> slint::Image {
    let y1 = y1.min(pm.height()).max(y0 + 1);
    let w = pm.width() as usize;
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(pm.width(), y1 - y0);
    buf.make_mut_bytes().copy_from_slice(&pm.data()[y0 as usize * w * 4..y1 as usize * w * 4]);
    slint::Image::from_rgba8_premultiplied(buf)
}

/// Path commands for the played part of the seek line (WavySeekPainter).
pub fn seek_wave(end_x: f32, height: f32, large: bool, clock: &WaveClock) -> String {
    let inset = 10.0;
    let y = height / 2.0;
    let amp = if large { 3.4 } else { 2.4 } * clock.energy;
    let wl = if large { 30.0 } else { 22.0 };
    let shift = clock.phase * 9.0;
    let mut s = String::with_capacity(((end_x - inset).max(0.0) / 2.0) as usize * 14 + 32);
    use std::fmt::Write;
    let _ = write!(s, "M {inset} {y:.2}");
    let mut px = inset;
    while px <= end_x {
        let taper = ((px - inset).min(end_x - px) / 10.0).min(1.0);
        let _ = write!(s, " L {:.1} {:.2}", px, y + amp * taper * ((px - shift) / wl * 2.0 * PI).sin());
        px += 2.0;
    }
    let _ = write!(s, " L {end_x:.1} {y:.2}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::{wave_colors, DEFAULT_ACCENT, FALLBACK_DEEP};

    #[test]
    fn clock_calms_down_and_stops() {
        let mut c = WaveClock::default();
        for _ in 0..100 {
            c.advance(0.05, true);
        }
        assert!(c.energy > 0.99);
        for _ in 0..400 {
            c.advance(0.05, false);
        }
        assert_eq!(c.energy, 0.0);
        assert!(!c.ticking(false));
    }

    #[test]
    fn renders_something() {
        let mut pm = Pixmap::new(120, 80).unwrap();
        let colors = wave_colors(DEFAULT_ACCENT, FALLBACK_DEEP, true);
        let spec = WaveSpec {
            intensity: 1.0,
            height_factor: 0.4,
            layers: 4,
            aurora: true,
            paint_base: true,
            glow: true,
            vignette: 0.5,
            y_off: 0.0,
            full_h: 0.0,
        };
        render(&mut pm, 1.0, &colors, &WaveClock { phase: 1.0, energy: 1.0, idle: 0.0 }, &spec);
        assert!(pm.data().iter().any(|&b| b != 0));
    }

    /// The fast rasteriser draws the same picture as the tiny-skia paths.
    #[test]
    fn fast_waves_match_tiny_skia() {
        for dark in [true, false] {
            let colors = wave_colors(DEFAULT_ACCENT, FALLBACK_DEEP, dark);
            for (glow, intensity, layers, px) in [(false, 0.38, 3, 0.33), (true, 1.0, 4, 0.5), (false, 0.45, 2, 0.5)] {
                for k in 0..4 {
                    let clock = WaveClock { phase: 0.9 + k as f32 * 1.3, energy: if k == 3 { 0.4 } else { 1.0 }, idle: 0.0 };
                    let spec = WaveSpec {
                        intensity,
                        height_factor: 0.42,
                        layers,
                        aurora: false,
                        paint_base: false,
                        glow,
                        vignette: 0.0,
                        y_off: 40.0,
                        full_h: 320.0,
                    };
                    let mut base = Pixmap::new(566, 280).unwrap();
                    base.fill(Color::from_rgba8(20, 24, 40, 255));
                    let mut a = base.clone();
                    let mut b = base.clone();
                    draw_waves(&mut a, px, &colors, &clock, &spec, Pass::All);
                    draw_waves_skia(&mut b, px, &colors, &clock, &spec);
                    let (mut sum, mut max, mut big) = (0u64, 0u8, 0usize);
                    for (x, y) in a.data().iter().zip(b.data()) {
                        let d = x.abs_diff(*y);
                        sum += d as u64;
                        max = max.max(d);
                        if d > 8 {
                            big += 1;
                        }
                    }
                    let mean = sum as f64 / a.data().len() as f64;
                    // ~1 level of rounding everywhere; a little more only on a
                    // few anti-aliased edge pixels of the glow strokes
                    assert!(mean < 1.0, "mean diff {mean} (glow {glow}, dark {dark})");
                    assert!(max <= 16, "max channel diff {max}");
                    assert!(big < a.data().len() / 500, "{big} channels differ by > 8");
                }
            }
        }
    }

    #[test]
    fn rows_outside_the_strip_never_change() {
        let colors = wave_colors(DEFAULT_ACCENT, FALLBACK_DEEP, true);
        for glow in [false, true] {
            let spec = WaveSpec {
                intensity: 0.6,
                height_factor: 0.42,
                layers: 4,
                aurora: false,
                paint_base: false,
                glow,
                vignette: 0.0,
                y_off: 0.0,
                full_h: 0.0,
            };
            let (w, h) = (160u32, 120u32);
            let (top, bottom) = wave_rows(&spec, colors.layers.len(), h as f32, 1.0);
            let (top, bottom) = (top.floor() as usize, bottom.ceil() as usize);
            assert!(top > 0 && bottom < h as usize && top < bottom);
            let mut first: Option<Vec<u8>> = None;
            for k in 0..12 {
                let mut pm = Pixmap::new(w, h).unwrap();
                render(&mut pm, 1.0, &colors, &WaveClock { phase: k as f32 * 0.7, energy: 1.0, idle: 0.0 }, &spec);
                let d = pm.data();
                let row = w as usize * 4;
                assert!(d[..top * row].iter().all(|&b| b == 0), "waves above the strip");
                let below = d[bottom * row..].to_vec();
                match &first {
                    None => first = Some(below),
                    Some(f) => assert!(f == &below, "rows below the strip changed"),
                }
            }
        }
    }

    /// `cargo test --release -- --ignored --nocapture wave_speed`
    #[test]
    #[ignore]
    fn wave_speed() {
        let colors = wave_colors(DEFAULT_ACCENT, FALLBACK_DEEP, true);
        for glow in [false, true] {
            let spec = WaveSpec {
                intensity: 1.0,
                height_factor: 0.42,
                layers: 4,
                aurora: false,
                paint_base: false,
                glow,
                vignette: 0.0,
                y_off: 250.0,
                full_h: 475.0,
            };
            let mut pm = Pixmap::new(850, 170).unwrap();
            let t0 = std::time::Instant::now();
            for k in 0..200 {
                pm.fill(Color::from_rgba8(20, 24, 40, 255));
                draw_waves(&mut pm, 0.5, &colors, &WaveClock { phase: k as f32 * 0.05, energy: 1.0, idle: 0.0 }, &spec, Pass::Moving);
            }
            eprintln!("glow {glow}: {:?} per frame", t0.elapsed() / 200);
        }
    }

    /// Static pass + moving pass = the whole picture, exactly.
    #[test]
    fn split_passes_add_up() {
        for dark in [true, false] {
            let colors = wave_colors(DEFAULT_ACCENT, FALLBACK_DEEP, dark);
            for (glow, intensity, layers, px) in [(false, 0.38, 3, 0.33), (true, 1.0, 4, 0.5)] {
                for k in 0..5 {
                    let clock = WaveClock { phase: 0.4 + k as f32 * 1.7, energy: if k == 4 { 0.3 } else { 1.0 }, idle: 0.0 };
                    let spec = WaveSpec {
                        intensity,
                        height_factor: 0.42,
                        layers,
                        aurora: false,
                        paint_base: false,
                        glow,
                        vignette: 0.0,
                        y_off: 150.0,
                        full_h: 320.0,
                    };
                    let mut base = Pixmap::new(300, 140).unwrap();
                    base.fill(Color::from_rgba8(30, 20, 50, 255));
                    let mut whole = base.clone();
                    draw_waves(&mut whole, px, &colors, &clock, &spec, Pass::All);
                    let mut split = base.clone();
                    draw_waves(&mut split, px, &colors, &clock, &spec, Pass::Static);
                    draw_waves(&mut split, px, &colors, &clock, &spec, Pass::Moving);
                    assert!(whole.data() == split.data(), "split differs (dark {dark}, glow {glow}, k {k})");
                }
            }
        }
    }

    /// A slice rendered through a column map equals those columns of the frame.
    #[test]
    fn column_slices_match_the_frame() {
        let colors = wave_colors(DEFAULT_ACCENT, FALLBACK_DEEP, true);
        let spec = WaveSpec {
            intensity: 1.0,
            height_factor: 0.42,
            layers: 4,
            aurora: false,
            paint_base: false,
            glow: true,
            vignette: 0.0,
            y_off: 120.0,
            full_h: 300.0,
        };
        let clock = WaveClock { phase: 2.2, energy: 1.0, idle: 0.0 };
        let mut whole = Pixmap::new(400, 150).unwrap();
        draw_waves(&mut whole, 0.5, &colors, &clock, &spec, Pass::All);
        let xmap: Vec<f32> = (150..270).map(|x| x as f32 + 0.5).collect();
        let mut part = Pixmap::new(120, 150).unwrap();
        render_waves_cols(&mut part, 0.5, &colors, &clock, &spec, Pass::Static, 400.0, &xmap);
        render_waves_cols(&mut part, 0.5, &colors, &clock, &spec, Pass::Moving, 400.0, &xmap);
        for y in 0..150usize {
            assert_eq!(&whole.data()[(y * 400 + 150) * 4..(y * 400 + 270) * 4], &part.data()[y * 120 * 4..(y + 1) * 120 * 4], "row {y}");
        }
    }

    #[test]
    fn upscale_keeps_flat_colours_and_corners() {
        let mut small = Pixmap::new(4, 3).unwrap();
        small.fill(Color::from_rgba8(10, 200, 30, 255));
        let mut big = Pixmap::new(17, 11).unwrap();
        upscale_into(&small, &mut big);
        assert!(big.data().chunks(4).all(|p| p == [10, 200, 30, 255]));
    }

    #[test]
    fn seek_wave_is_a_path() {
        let s = seek_wave(100.0, 22.0, false, &WaveClock { phase: 0.3, energy: 1.0, idle: 0.0 });
        assert!(s.starts_with("M 10 11.00"));
        assert!(s.ends_with("L 100.0 11.00"));
    }
}
