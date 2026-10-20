//! Colours: cover palette, accent tuning, desktop (Caelestia) scheme and the
//! Geonix palette — ports of theme.dart, palette.dart and desktop_theme.dart.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const fn hex(v: u32) -> Rgba {
        Rgba { r: ((v >> 16) & 0xff) as f32 / 255.0, g: ((v >> 8) & 0xff) as f32 / 255.0, b: (v & 0xff) as f32 / 255.0, a: 1.0 }
    }
    pub const fn argb(v: u32) -> Rgba {
        Rgba {
            r: ((v >> 16) & 0xff) as f32 / 255.0,
            g: ((v >> 8) & 0xff) as f32 / 255.0,
            b: (v & 0xff) as f32 / 255.0,
            a: ((v >> 24) & 0xff) as f32 / 255.0,
        }
    }
    pub fn rgb8(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
    }
    pub fn alpha(self, a: f32) -> Rgba {
        Rgba { a: a.clamp(0.0, 1.0), ..self }
    }
    pub fn lerp(self, o: Rgba, t: f32) -> Rgba {
        Rgba {
            r: self.r + (o.r - self.r) * t,
            g: self.g + (o.g - self.g) * t,
            b: self.b + (o.b - self.b) * t,
            a: self.a + (o.a - self.a) * t,
        }
    }
    pub fn to_slint(self) -> slint::Color {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        slint::Color::from_argb_u8(q(self.a), q(self.r), q(self.g), q(self.b))
    }
    pub fn to_skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(self.r.clamp(0.0, 1.0), self.g.clamp(0.0, 1.0), self.b.clamp(0.0, 1.0), self.a.clamp(0.0, 1.0))
            .unwrap_or(tiny_skia::Color::BLACK)
    }
    /// Flutter's ThemeData.estimateBrightnessForColor: true when dark.
    pub fn is_dark(self) -> bool {
        let lin = |c: f32| if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        let l = 0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b);
        (l + 0.05) * (l + 0.05) <= 0.15
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Hsl {
    pub h: f32,
    pub s: f32,
    pub l: f32,
    pub a: f32,
}

impl Hsl {
    pub fn from(c: Rgba) -> Hsl {
        let max = c.r.max(c.g).max(c.b);
        let min = c.r.min(c.g).min(c.b);
        let d = max - min;
        let l = (max + min) / 2.0;
        let h = if d == 0.0 {
            0.0
        } else if max == c.r {
            60.0 * (((c.g - c.b) / d) % 6.0)
        } else if max == c.g {
            60.0 * ((c.b - c.r) / d + 2.0)
        } else {
            60.0 * ((c.r - c.g) / d + 4.0)
        };
        let h = if h.is_nan() { 0.0 } else { (h + 360.0) % 360.0 };
        let s = if l == 1.0 || l == 0.0 { 0.0 } else { (d / (1.0 - (2.0 * l - 1.0).abs())).clamp(0.0, 1.0) };
        Hsl { h, s, l, a: c.a }
    }
    pub fn to_rgba(self) -> Rgba {
        let c = (1.0 - (2.0 * self.l - 1.0).abs()) * self.s;
        let hp = self.h / 60.0;
        let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
        let (r, g, b) = match hp as i32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = self.l - c / 2.0;
        Rgba { r: r + m, g: g + m, b: b + m, a: self.a }
    }
}

pub fn shift_hue(c: Rgba, degrees: f32, lightness: f32) -> Rgba {
    let mut h = Hsl::from(c);
    h.h = (h.h + degrees).rem_euclid(360.0);
    h.l = (h.l + lightness).clamp(0.0, 1.0);
    h.to_rgba()
}

pub const DEFAULT_ACCENT: Rgba = Rgba::hex(0x8B7CFF);
pub const FALLBACK_DEEP: Rgba = Rgba::hex(0x1A1730);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoverPalette {
    pub accent: Rgba,
    pub deep: Rgba,
}

impl CoverPalette {
    pub const FALLBACK: CoverPalette = CoverPalette { accent: DEFAULT_ACCENT, deep: FALLBACK_DEEP };
}

/// 18 hue buckets weighted by saturation and mid-brightness; heaviest wins.
pub fn palette_from_pixels(rgba: &[u8]) -> CoverPalette {
    const B: usize = 18;
    let mut weight = [0f64; B];
    let mut sr = [0f64; B];
    let mut sg = [0f64; B];
    let mut sb = [0f64; B];
    let (mut ar, mut ag, mut ab, mut count) = (0f64, 0f64, 0f64, 0usize);
    for p in rgba.as_chunks::<4>().0 {
        if p[3] < 128 {
            continue;
        }
        let (r, g, b) = (p[0], p[1], p[2]);
        ar += r as f64;
        ag += g as f64;
        ab += b as f64;
        count += 1;
        let hsl = Hsl::from(Rgba::rgb8(r, g, b));
        if hsl.s < 0.22 || hsl.l < 0.12 || hsl.l > 0.9 {
            continue;
        }
        let w = (hsl.s * (1.0 - (hsl.l - 0.5).abs() * 1.4)) as f64;
        if w <= 0.0 {
            continue;
        }
        let k = ((hsl.h / 360.0 * B as f32).floor() as usize) % B;
        weight[k] += w;
        sr[k] += r as f64 * w;
        sg[k] += g as f64 * w;
        sb[k] += b as f64 * w;
    }
    if count == 0 {
        return CoverPalette::FALLBACK;
    }
    let n = count as f64;
    let avg = Rgba::rgb8((ar / n) as u8, (ag / n) as u8, (ab / n) as u8);
    let mut ah = Hsl::from(avg);
    ah.l = 0.13;
    ah.s = (ah.s * 0.8).clamp(0.0, 0.6);
    let deep = ah.to_rgba();
    let best = (0..B).max_by(|&a, &b| weight[a].partial_cmp(&weight[b]).unwrap()).unwrap();
    if weight[best] < n * 0.02 {
        return CoverPalette { accent: DEFAULT_ACCENT, deep };
    }
    let w = weight[best];
    let accent = Rgba::rgb8((sr[best] / w).round() as u8, (sg[best] / w).round() as u8, (sb[best] / w).round() as u8);
    let mut dh = Hsl::from(accent);
    dh.l = 0.14;
    dh.s = 0.45;
    CoverPalette { accent, deep: deep.lerp(dh.to_rgba(), 0.6) }
}

/// Keep cover accents readable: bright enough on dark, deep enough on light.
pub fn tune_accent(c: Rgba, dark: bool) -> Rgba {
    let mut h = Hsl::from(c);
    h.s = h.s.clamp(0.45, 0.9);
    h.l = if dark { h.l.clamp(0.58, 0.72) } else { h.l.clamp(0.38, 0.5) };
    h.to_rgba()
}

/// Pull a cover hue toward the desktop primary hue (at most 25°).
pub fn harmonize(cover: Rgba, primary: Rgba) -> Rgba {
    let mut c = Hsl::from(cover);
    let p = Hsl::from(primary);
    let mut diff = (p.h - c.h).rem_euclid(360.0);
    if diff > 180.0 {
        diff -= 360.0;
    }
    let shift = (diff * 0.5).clamp(-25.0, 25.0);
    c.h = (c.h + shift).rem_euclid(360.0);
    c.to_rgba()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopScheme {
    pub dark: bool,
    pub background: Rgba,
    pub surface: Rgba,
    pub surface_high: Rgba,
    pub primary: Rgba,
    pub on_surface: Rgba,
    pub on_surface_variant: Rgba,
    pub outline: Rgba,
}

impl DesktopScheme {
    pub fn parse(src: &str) -> Option<DesktopScheme> {
        let v: serde_json::Value = serde_json::from_str(src).ok()?;
        let colours = v.get("colours").or_else(|| v.get("colors"))?.as_object()?;
        let c = |keys: &[&str]| -> Option<Rgba> {
            for k in keys {
                if let Some(s) = colours.get(*k).and_then(|x| x.as_str()) {
                    let hex = s.trim().trim_start_matches('#');
                    if hex.len() == 6 {
                        if let Ok(n) = u32::from_str_radix(hex, 16) {
                            return Some(Rgba::hex(n));
                        }
                    }
                }
            }
            None
        };
        let background = c(&["background", "surface"])?;
        let primary = c(&["primary"])?;
        let on_surface = c(&["onSurface", "onBackground"])?;
        let dark = match v.get("mode").and_then(|m| m.as_str()) {
            Some("light") => false,
            Some("dark") => true,
            _ => background.is_dark(),
        };
        Some(DesktopScheme {
            dark,
            background,
            surface: c(&["surfaceContainer", "surface"]).unwrap_or(background),
            surface_high: c(&["surfaceContainerHigh", "surfaceContainerHighest", "surfaceVariant"]).unwrap_or(background),
            primary,
            on_surface,
            on_surface_variant: c(&["onSurfaceVariant", "outline"]).unwrap_or(on_surface.alpha(0.7)),
            outline: c(&["outlineVariant", "outline"]).unwrap_or(on_surface.alpha(0.2)),
        })
    }

    /// Dark tone of the scheme for wave backgrounds when nothing plays.
    pub fn deep(&self) -> Rgba {
        self.background.lerp(self.primary, if self.dark { 0.14 } else { 0.10 })
    }
}

pub fn desktop_scheme_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GEONIX_SCHEME_FILE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"));
    base.join("caelestia/scheme.json")
}

/// The app palette (GeonixPalette) for the current theme and song.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub background: Rgba,
    pub surface: Rgba,
    pub surface_high: Rgba,
    pub outline: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub accent: Rgba,
    pub on_accent: Rgba,
    pub deep: Rgba,
}

/// `cover` is the playing song's palette (None when nothing plays).
pub fn build_palette(cover: Option<CoverPalette>, desktop: Option<&DesktopScheme>, dark_mode: bool) -> Palette {
    // currentPaletteProvider + accentProvider
    let cp = cover.unwrap_or(CoverPalette::FALLBACK);
    let (raw_accent, deep) = match desktop {
        Some(d) if cover.is_none() || cover == Some(CoverPalette::FALLBACK) => (d.primary, d.deep()),
        Some(d) => (harmonize(cp.accent, d.primary), cp.deep.lerp(d.deep(), 0.45)),
        None => (if cover.is_none() { DEFAULT_ACCENT } else { cp.accent }, cp.deep),
    };
    let dark = desktop.map(|d| d.dark).unwrap_or(dark_mode);
    let accent = tune_accent(raw_accent, dark);
    let on_accent = if accent.is_dark() { Rgba::hex(0xFFFFFF) } else { Rgba::hex(0x0B0C10) };
    match desktop {
        Some(d) => Palette {
            dark,
            background: d.background,
            surface: d.surface,
            surface_high: d.surface_high,
            outline: d.outline.alpha(0.45 * d.outline.a),
            text: d.on_surface,
            muted: d.on_surface_variant,
            accent,
            on_accent,
            deep,
        },
        None => Palette {
            dark,
            background: if dark { Rgba::hex(0x0B0C10) } else { Rgba::hex(0xF5F5F8) },
            surface: if dark { Rgba::hex(0x15161C) } else { Rgba::hex(0xFFFFFF) },
            surface_high: if dark { Rgba::hex(0x1F2029) } else { Rgba::hex(0xECECF2) },
            outline: if dark { Rgba::argb(0x1FFFFFFF) } else { Rgba::argb(0x1A000000) },
            text: if dark { Rgba::hex(0xF3F3F7) } else { Rgba::hex(0x121218) },
            muted: if dark { Rgba::hex(0x9A9BA8) } else { Rgba::hex(0x686977) },
            accent,
            on_accent,
            deep,
        },
    }
}

/// Colours for the waves/aurora (WaveColors.from).
pub struct WaveColors {
    pub base: Rgba,
    pub aurora: [Rgba; 3],
    pub layers: [Rgba; 4],
}

pub fn wave_colors(accent: Rgba, deep: Rgba, dark: bool) -> WaveColors {
    if dark {
        return WaveColors {
            base: deep.lerp(Rgba::hex(0), 0.35),
            aurora: [accent.alpha(0.55), shift_hue(accent, 38.0, 0.05).alpha(0.35), shift_hue(deep, -20.0, 0.12).alpha(0.7)],
            layers: [
                shift_hue(accent, -28.0, 0.0).alpha(0.20),
                accent.alpha(0.24),
                shift_hue(accent, 32.0, 0.08).alpha(0.22),
                accent.lerp(Rgba::hex(0xFFFFFF), 0.3).alpha(0.16),
            ],
        };
    }
    let mut sh = Hsl::from(accent);
    sh.l = 0.62;
    let soft = sh.to_rgba();
    WaveColors {
        base: Rgba::hex(0xF5F5F8).lerp(accent, 0.07),
        aurora: [soft.alpha(0.30), shift_hue(soft, 38.0, 0.0).alpha(0.22), shift_hue(soft, -30.0, 0.0).alpha(0.18)],
        layers: [
            shift_hue(soft, -28.0, 0.0).alpha(0.14),
            soft.alpha(0.17),
            shift_hue(soft, 32.0, 0.0).alpha(0.15),
            soft.lerp(Rgba::hex(0xFFFFFF), 0.3).alpha(0.14),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_roundtrip() {
        let c = Rgba::hex(0x8B7CFF);
        let back = Hsl::from(c).to_rgba();
        assert!((c.r - back.r).abs() < 0.01 && (c.g - back.g).abs() < 0.01 && (c.b - back.b).abs() < 0.01);
    }

    #[test]
    fn grey_cover_keeps_default_accent() {
        let px: Vec<u8> = std::iter::repeat_n([128u8, 128, 128, 255], 100).flatten().collect();
        assert_eq!(palette_from_pixels(&px).accent, DEFAULT_ACCENT);
    }

    #[test]
    fn red_cover_gives_red_accent() {
        let px: Vec<u8> = std::iter::repeat_n([200u8, 30, 30, 255], 100).flatten().collect();
        let p = palette_from_pixels(&px);
        assert!(p.accent.r > 0.7 && p.accent.g < 0.2);
    }

    #[test]
    fn parses_caelestia_scheme() {
        let s = r#"{"mode":"dark","colours":{"background":"131315","onSurface":"e4e2e4","primary":"b4c5ff","surfaceContainer":"1f1f21"}}"#;
        let d = DesktopScheme::parse(s).unwrap();
        assert!(d.dark);
        assert_eq!(d.surface, Rgba::hex(0x1f1f21));
        assert!(DesktopScheme::parse("{}").is_none());
    }
}
