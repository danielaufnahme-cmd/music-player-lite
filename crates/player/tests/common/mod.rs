#![allow(dead_code)]
//! Test helpers: generated audio files (no audible output anywhere) and a
//! tiny HTTP server with Range support.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub const RATE: u32 = 44100;

/// Scratch root for generated audio: ~/.cache/geonix-lite (falls back to the OS temp dir).
pub fn test_root() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/geonix-lite")).unwrap_or_else(std::env::temp_dir)
}

pub fn temp_dir(name: &str) -> PathBuf {
    let d = test_root().join(format!("player-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 16-bit stereo WAV. `f(frame) -> (left, right)` in -1..1.
pub fn write_wav(path: &Path, frames: usize, f: impl Fn(usize) -> (f32, f32)) {
    let mut data = Vec::with_capacity(frames * 4);
    for i in 0..frames {
        let (l, r) = f(i);
        for s in [l, r] {
            data.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    std::fs::write(path, out).unwrap();
}

pub fn sine(freq: f32, amp: f32) -> impl Fn(usize) -> (f32, f32) {
    move |i| {
        let v = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / RATE as f32).sin();
        (v, v)
    }
}

/// Silence until `onset` seconds, then a 1 kHz tone.
pub fn onset(onset: f64) -> impl Fn(usize) -> (f32, f32) {
    let start = (onset * RATE as f64) as usize;
    move |i| {
        if i < start {
            (0.0, 0.0)
        } else {
            let v = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / RATE as f32).sin();
            (v, v)
        }
    }
}

/// Encodes `wav` with ffmpeg into `out` (format from extension).
pub fn encode(wav: &Path, out: &Path) {
    let codec: &[&str] = match out.extension().unwrap().to_str().unwrap() {
        "mp3" => &["-c:a", "libmp3lame", "-b:a", "192k"],
        "m4a" => &["-c:a", "aac", "-b:a", "160k"],
        "flac" => &["-c:a", "flac"],
        "ogg" => &["-c:a", "libvorbis", "-q:a", "5"],
        e => panic!("ext {e}"),
    };
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(wav)
        .args(codec)
        .arg(out)
        .status()
        .expect("ffmpeg");
    assert!(st.success(), "ffmpeg failed for {}", out.display());
}

/// Generates `name.wav` (+ encoded variants) and returns paths per extension.
pub fn make_set(dir: &Path, name: &str, secs: f64, f: impl Fn(usize) -> (f32, f32), exts: &[&str]) -> Vec<PathBuf> {
    let wav = dir.join(format!("{name}.wav"));
    write_wav(&wav, (secs * RATE as f64) as usize, f);
    exts.iter()
        .map(|e| {
            if *e == "wav" {
                wav.clone()
            } else {
                let p = dir.join(format!("{name}.{e}"));
                encode(&wav, &p);
                p
            }
        })
        .collect()
}

pub struct TestServer {
    pub base: String,
    pub requests: Arc<AtomicUsize>,
    pub range_requests: Arc<AtomicUsize>,
}

/// Serves files from `dir` at `/name`. With `ranges=false` it ignores Range (200 only).
pub fn serve(dir: PathBuf, ranges: bool) -> TestServer {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let range_requests = Arc::new(AtomicUsize::new(0));
    let (rq, rr) = (requests.clone(), range_requests.clone());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut conn) = conn else { continue };
            let dir = dir.clone();
            let (rq, rr) = (rq.clone(), rr.clone());
            std::thread::spawn(move || {
                let mut rd = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                if rd.read_line(&mut line).is_err() {
                    return;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap().to_string();
                let mut range: Option<(u64, Option<u64>)> = None;
                loop {
                    let mut h = String::new();
                    if rd.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("range: bytes=") {
                        let (a, b) = v.trim().split_once('-').unwrap();
                        range = Some((a.parse().unwrap(), b.parse().ok()));
                    }
                }
                rq.fetch_add(1, Ordering::SeqCst);
                let file = dir.join(path.trim_start_matches('/'));
                let Ok(data) = std::fs::read(&file) else {
                    let _ = conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                };
                let len = data.len() as u64;
                let ct = match file.extension().and_then(|e| e.to_str()) {
                    Some("mp3") => "audio/mpeg",
                    Some("m4a") => "audio/mp4",
                    _ => "application/octet-stream",
                };
                let (status, body, extra) = match (ranges, range) {
                    (true, Some((a, b))) => {
                        rr.fetch_add(1, Ordering::SeqCst);
                        if a >= len {
                            let _ = conn.write_all(
                                format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
                            );
                            return;
                        }
                        let b = b.unwrap_or(len - 1).min(len - 1);
                        (
                            "206 Partial Content",
                            &data[a as usize..=b as usize],
                            format!("Content-Range: bytes {a}-{b}/{len}\r\nAccept-Ranges: bytes\r\n"),
                        )
                    }
                    _ => ("200 OK", &data[..], String::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(body);
            });
        }
    });
    TestServer { base, requests, range_requests }
}
