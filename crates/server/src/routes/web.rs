//! The phone download page, embedded in the binary and served at `/`.
//! Reach it on the tailnet via `tailscale serve`; it only calls `/api/*`
//! on its own origin.

use axum::http::header;
use axum::response::IntoResponse;

const INDEX: &str = include_str!("../../web/index.html");
const MANIFEST: &str = include_str!("../../web/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../../web/sw.js");
const ICON_192: &[u8] = include_bytes!("../../web/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../../web/icon-512.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../../web/apple-touch-icon.png");

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                   script-src 'self' 'unsafe-inline'; connect-src 'self'; manifest-src 'self'; \
                   worker-src 'self'; frame-ancestors 'none'";

pub async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        INDEX,
    )
}

pub async fn manifest() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/manifest+json"), (header::CACHE_CONTROL, "no-cache")], MANIFEST)
}

pub async fn service_worker() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], SERVICE_WORKER)
}

fn png(bytes: &'static [u8]) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=604800")], bytes)
}

pub async fn icon_192() -> impl IntoResponse {
    png(ICON_192)
}

pub async fn icon_512() -> impl IntoResponse {
    png(ICON_512)
}

pub async fn apple_touch_icon() -> impl IntoResponse {
    png(APPLE_TOUCH_ICON)
}
