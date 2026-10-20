//! Song covers: fetched from the server once per (song, display size),
//! decoded straight to the size they are shown at with the rounded corners
//! baked in, and kept in a small byte-bounded LRU. Full-size originals are
//! never kept.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};

use crate::api::Api;
use crate::palette::{palette_from_pixels, CoverPalette};
use crate::{AppWindow, Covers};

/// Decoded covers kept around (MB of RGBA pixels); the "Cover cache" setting.
static LIMIT_MB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(24);

pub fn set_limit_mb(mb: u32) {
    LIMIT_MB.store((mb as usize).clamp(4, 512), std::sync::atomic::Ordering::Relaxed);
}

#[allow(non_snake_case)]
fn LIMIT_BYTES() -> usize {
    LIMIT_MB.load(std::sync::atomic::Ordering::Relaxed) * 1024 * 1024
}

type Key = (String, u32, u32);

struct Entry {
    image: Image,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<Key, Entry>,
    bytes: usize,
    tick: u64,
    pending: HashSet<Key>,
    failed: HashSet<String>,
    bump_scheduled: bool,
}

struct Ctx {
    win: slint::Weak<AppWindow>,
    api: Api,
    rt: tokio::runtime::Handle,
    sem: Arc<tokio::sync::Semaphore>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

pub fn init(win: &AppWindow, api: Api, rt: tokio::runtime::Handle) {
    CTX.with(|c| *c.borrow_mut() = Some(Ctx { win: win.as_weak(), api, rt, sem: Arc::new(tokio::sync::Semaphore::new(4)) }));
    let weak = win.as_weak();
    win.global::<Covers>().on_get(move |id, size, radius, _rev| {
        let scale = weak.upgrade().map(|w| w.window().scale_factor()).unwrap_or(1.0);
        get(id.as_str(), size, radius, scale)
    });
}

/// Forget failures so covers of newly downloaded songs are tried again.
pub fn retry_failed() {
    CACHE.with(|c| c.borrow_mut().failed.clear());
}

fn get(id: &str, size: f32, radius: f32, scale: f32) -> Image {
    if id.is_empty() || size <= 0.0 {
        return Image::default();
    }
    let px = ((size * scale).round() as u32).clamp(8, 1024);
    let rad = ((radius * scale).round() as u32).min(px / 2);
    let key: Key = (id.to_string(), px, rad);
    let start = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick += 1;
        let tick = c.tick;
        if let Some(e) = c.map.get_mut(&key) {
            e.used = tick;
            return Err(e.image.clone());
        }
        if c.failed.contains(id) || c.pending.contains(&key) {
            return Err(Image::default());
        }
        c.pending.insert(key.clone());
        Ok(())
    });
    if let Err(img) = start {
        return img;
    }
    CTX.with(|ctx| {
        let ctx = ctx.borrow();
        let Some(ctx) = ctx.as_ref() else { return };
        let api = ctx.api.clone();
        let sem = ctx.sem.clone();
        let key2 = key.clone();
        ctx.rt.spawn(async move {
            let _permit = sem.acquire().await;
            let bytes = api.cover_bytes(&key2.0).await;
            let decoded = match bytes {
                Some(b) => tokio::task::spawn_blocking(move || decode(&b, key2.1, key2.2)).await.ok().flatten(),
                None => None,
            };
            let _ = slint::invoke_from_event_loop(move || insert(key2, decoded));
        });
    });
    Image::default()
}

fn insert(key: Key, buf: Option<SharedPixelBuffer<Rgba8Pixel>>) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.pending.remove(&key);
        match buf {
            None => {
                c.failed.insert(key.0.clone());
            }
            Some(buf) => {
                let bytes = (buf.width() * buf.height() * 4) as usize;
                c.tick += 1;
                let used = c.tick;
                c.bytes += bytes;
                c.map.insert(key, Entry { image: Image::from_rgba8_premultiplied(buf), bytes, used });
                if c.bytes > LIMIT_BYTES() {
                    let mut by_age: Vec<(u64, Key)> = c.map.iter().map(|(k, e)| (e.used, k.clone())).collect();
                    by_age.sort();
                    for (_, k) in by_age {
                        if c.bytes <= LIMIT_BYTES() * 3 / 4 {
                            break;
                        }
                        if let Some(e) = c.map.remove(&k) {
                            c.bytes -= e.bytes;
                        }
                    }
                }
            }
        }
        if !c.bump_scheduled {
            c.bump_scheduled = true;
            // Coalesce: one re-evaluation of cover bindings per 60 ms.
            slint::Timer::single_shot(std::time::Duration::from_millis(60), || {
                CACHE.with(|c| c.borrow_mut().bump_scheduled = false);
                CTX.with(|ctx| {
                    if let Some(w) = ctx.borrow().as_ref().and_then(|c| c.win.upgrade()) {
                        let g = w.global::<Covers>();
                        g.set_rev(g.get_rev().wrapping_add(1));
                    }
                });
            });
        }
    });
}

/// Decode, crop to a square of `px`, round the corners (radius `rad`) and premultiply.
pub fn decode(bytes: &[u8], px: u32, rad: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = img.resize_to_fill(px, px, image::imageops::FilterType::Triangle).to_rgba8();
    let (w, h) = img.dimensions();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    let out = buf.make_mut_bytes();
    let raw = img.as_raw();
    let r = rad as f32;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let mut cov = 1.0f32;
            if rad > 0 {
                // distance from the nearest corner centre
                let fx = x as f32 + 0.5;
                let fy = y as f32 + 0.5;
                let cx = if fx < r {
                    r
                } else if fx > w as f32 - r {
                    w as f32 - r
                } else {
                    fx
                };
                let cy = if fy < r {
                    r
                } else if fy > h as f32 - r {
                    h as f32 - r
                } else {
                    fy
                };
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                cov = (r - d + 0.5).clamp(0.0, 1.0);
            }
            let a = raw[i + 3] as f32 / 255.0 * cov;
            out[i] = (raw[i] as f32 * a) as u8;
            out[i + 1] = (raw[i + 1] as f32 * a) as u8;
            out[i + 2] = (raw[i + 2] as f32 * a) as u8;
            out[i + 3] = (a * 255.0) as u8;
        }
    }
    Some(buf)
}

/// Colours of a cover (decoded at 40×40 only).
pub async fn palette(api: &Api, id: &str) -> CoverPalette {
    let Some(bytes) = api.cover_bytes(id).await else { return CoverPalette::FALLBACK };
    tokio::task::spawn_blocking(move || {
        image::load_from_memory(&bytes)
            .ok()
            .map(|i| palette_from_pixels(i.resize_exact(40, 40, image::imageops::FilterType::Triangle).to_rgba8().as_raw()))
            .unwrap_or(CoverPalette::FALLBACK)
    })
    .await
    .unwrap_or(CoverPalette::FALLBACK)
}

#[cfg(test)]
mod tests {
    #[test]
    fn decodes_with_round_corners() {
        let img = image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let buf = super::decode(&png, 32, 8).unwrap();
        assert_eq!(buf.width(), 32);
        let px = buf.as_slice();
        assert_eq!(px[0].a, 0); // corner is transparent
        assert_eq!(px[16 * 32 + 16].a, 255);
    }
}
