//! End-to-end Player tests on the silent null sink (no audible output).

mod common;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use common::*;
use geonix_player::{NullBackend, PlaybackState, Player, PlayerEvent, PlayerOptions, Source};

fn player(backend: NullBackend, idle: Duration) -> (Player, Receiver<PlayerEvent>) {
    let mut o = PlayerOptions::null(backend);
    o.idle_release = idle;
    Player::with_options(o)
}

/// Waits for an event matching `pred`, collecting everything seen.
fn wait_for(
    rx: &Receiver<PlayerEvent>,
    secs: f64,
    seen: &mut Vec<(Instant, PlayerEvent)>,
    pred: impl Fn(&PlayerEvent) -> bool,
) -> PlayerEvent {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(e) => {
                seen.push((Instant::now(), e.clone()));
                if pred(&e) {
                    return e;
                }
            }
            Err(_) => panic!("timeout; events so far: {:?}", seen.iter().map(|x| &x.1).collect::<Vec<_>>()),
        }
    }
}

fn wav(name: &str, secs: f64) -> std::path::PathBuf {
    let dir = temp_dir(name);
    make_set(&dir, "t", secs, sine(440.0, 0.5), &["wav"]).remove(0)
}

#[test]
fn plays_to_end_with_events() {
    let f = wav("p-end", 2.0);
    let backend = NullBackend::with_speed(2.0);
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_secs(30));
    let mut seen = vec![];
    let t0 = Instant::now();
    p.load(Source::file(&f), true);
    match wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::Loaded { .. })) {
        PlayerEvent::Loaded { duration } => assert!((duration.unwrap() - 2.0).abs() < 1e-6),
        _ => unreachable!(),
    }
    wait_for(&rx, 5.0, &mut seen, |e| *e == PlayerEvent::Ended);
    let wall = t0.elapsed().as_secs_f64();
    assert_eq!(p.state(), PlaybackState::Stopped);
    assert!((p.position() - 2.0).abs() < 1e-6, "pos {}", p.position());
    // Every sample rendered exactly once.
    assert_eq!(stats.samples.load(Ordering::SeqCst), 2 * 2 * RATE as u64);
    let states: Vec<_> = seen.iter().filter_map(|(_, e)| if let PlayerEvent::State(s) = e { Some(*s) } else { None }).collect();
    assert_eq!(states, vec![PlaybackState::Loading, PlaybackState::Paused, PlaybackState::Playing, PlaybackState::Stopped]);
    // Position ticks: <= 4 Hz of wall time, monotonic.
    let pos: Vec<f64> =
        seen.iter().filter_map(|(_, e)| if let PlayerEvent::Position { position, .. } = e { Some(*position) } else { None }).collect();
    assert!(pos.len() as f64 <= wall * 4.0 + 3.0, "{} ticks in {wall:.2}s", pos.len());
    assert!(pos.len() >= 2);
    assert!(pos.windows(2).all(|w| w[1] >= w[0]), "{pos:?}");
    // Nothing more arrives once ended (no ticks while not playing).
    assert!(rx.recv_timeout(Duration::from_millis(700)).is_err());
}

#[test]
fn pause_releases_device_and_resumes() {
    let f = wav("p-pause", 6.0);
    let backend = NullBackend::new();
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_millis(400));
    let mut seen = vec![];
    p.load(Source::file(&f), true);
    wait_for(&rx, 5.0, &mut seen, |e| *e == PlayerEvent::State(PlaybackState::Playing));
    assert_eq!(stats.open_streams.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(600));
    p.pause();
    wait_for(&rx, 2.0, &mut seen, |e| *e == PlayerEvent::State(PlaybackState::Paused));
    wait_for(&rx, 2.0, &mut seen, |e| matches!(e, PlayerEvent::Position { .. })); // one final position
    let paused_at = p.position();
    assert!(paused_at > 0.4 && paused_at < 1.0, "{paused_at}");
    // Still open right after pausing, released after idle_release.
    assert_eq!(stats.open_streams.load(Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(stats.open_streams.load(Ordering::SeqCst), 0, "device not released");
    // No events while paused.
    assert!(rx.try_recv().is_err());
    assert!((p.position() - paused_at).abs() < 1e-9);

    p.play();
    wait_for(&rx, 2.0, &mut seen, |e| *e == PlayerEvent::State(PlaybackState::Playing));
    assert_eq!(stats.open_streams.load(Ordering::SeqCst), 1);
    assert_eq!(stats.opened_total.load(Ordering::SeqCst), 2);
    std::thread::sleep(Duration::from_millis(300));
    let pos = p.position();
    assert!(pos > paused_at + 0.15 && pos < paused_at + 0.6, "resumed at {pos}, paused at {paused_at}");

    p.stop();
    wait_for(&rx, 2.0, &mut seen, |e| *e == PlayerEvent::State(PlaybackState::Idle));
    assert_eq!(stats.open_streams.load(Ordering::SeqCst), 0);
    assert_eq!(p.position(), 0.0);
    assert_eq!(p.duration(), None);
}

#[test]
fn seek_while_paused_and_playing() {
    let dir = temp_dir("p-seek");
    let f = make_set(&dir, "o", 8.0, onset(5.0), &["mp3"]).remove(0);
    let backend = NullBackend::new().capture();
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_secs(30));
    let mut seen = vec![];
    p.load(Source::file(&f), false);
    wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::Loaded { .. }));
    assert_eq!(p.state(), PlaybackState::Paused);
    p.seek(4.5);
    wait_for(&rx, 2.0, &mut seen, |e| matches!(e, PlayerEvent::Position { position, .. } if (*position - 4.5).abs() < 0.01));
    assert_eq!(p.state(), PlaybackState::Paused);
    assert!((p.position() - 4.5).abs() < 0.01);
    stats.capture.lock().unwrap().as_mut().unwrap().clear();
    p.play();
    std::thread::sleep(Duration::from_millis(900));
    p.pause();
    let cap = stats.capture.lock().unwrap().take().unwrap();
    let first = cap.iter().position(|s| s.abs() > 0.1).expect("tone not reached") / 2;
    let at = 4.5 + first as f64 / RATE as f64;
    assert!((at - 5.0).abs() < 0.02, "tone heard at {at}");
    // Seek past the end clamps to just before it.
    p.seek(1000.0);
    wait_for(&rx, 2.0, &mut seen, |e| matches!(e, PlayerEvent::Position { position, .. } if *position > 7.9));
}

#[test]
fn preload_is_gapless() {
    let a = wav("p-gap-a", 1.0);
    let dir = temp_dir("p-gap-b");
    let b = make_set(&dir, "b", 1.5, sine(880.0, 0.25), &["wav"]).remove(0);
    let backend = NullBackend::with_speed(2.0).capture();
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_secs(30));
    let mut seen = vec![];
    p.load(Source::file(&a), true);
    wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::Loaded { .. }));
    p.preload(Some(Source::file(&b)));
    match wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::AdvancedToPreload { .. })) {
        PlayerEvent::AdvancedToPreload { duration } => assert!((duration.unwrap() - 1.5).abs() < 1e-6),
        _ => unreachable!(),
    }
    assert!(p.position() < 0.3, "position after hand-over {}", p.position());
    wait_for(&rx, 5.0, &mut seen, |e| *e == PlayerEvent::Ended);
    assert_eq!(seen.iter().filter(|(_, e)| *e == PlayerEvent::Ended).count(), 1);
    // A and B back to back, no silence between them.
    let cap = stats.capture.lock().unwrap().take().unwrap();
    assert_eq!(cap.len(), (2.5 * RATE as f64) as usize * 2);
    let seam = RATE as usize * 2;
    let around = &cap[seam - 200..seam + 200];
    let zeros = around.windows(8).filter(|w| w.iter().all(|s| *s == 0.0)).count();
    assert_eq!(zeros, 0, "gap at the seam");
    assert!((p.position() - 1.5).abs() < 1e-6);
}

#[test]
fn resamples_when_device_rate_differs() {
    let f = wav("p-rs", 1.0);
    let backend = NullBackend::with_speed(2.0).with_rate(48000);
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_secs(30));
    let mut seen = vec![];
    p.load(Source::file(&f), true);
    wait_for(&rx, 5.0, &mut seen, |e| *e == PlayerEvent::Ended);
    let n = stats.samples.load(Ordering::SeqCst) as i64;
    assert!((n - 96000).abs() <= 4, "{n}");
    assert!((p.position() - 1.0).abs() < 0.01);
}

#[test]
fn errors_and_toggle() {
    let (p, rx) = player(NullBackend::new(), Duration::from_secs(30));
    let mut seen = vec![];
    p.load(Source::file("/nonexistent/x.mp3"), true);
    wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::Error(_)));
    assert_eq!(p.state(), PlaybackState::Stopped);
    p.play(); // nothing loaded: no-op, no panic
    let f = wav("p-toggle", 3.0);
    p.load(Source::file(&f), false);
    wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::Loaded { .. }));
    p.toggle();
    wait_for(&rx, 2.0, &mut seen, |e| *e == PlayerEvent::State(PlaybackState::Playing));
    p.toggle();
    wait_for(&rx, 2.0, &mut seen, |e| *e == PlayerEvent::State(PlaybackState::Paused));
    p.set_volume(0.5);
    assert_eq!(p.volume(), 0.5);
    // Dropping the last handle shuts the engine down cleanly.
    let p2 = p.clone();
    drop(p);
    assert_eq!(p2.state(), PlaybackState::Paused);
    drop(p2);
}

#[test]
fn volume_scales_output() {
    let f = wav("p-vol", 0.5);
    let backend = NullBackend::with_speed(2.0).capture();
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_secs(30));
    p.set_volume(0.5);
    let mut seen = vec![];
    p.load(Source::file(&f), true);
    wait_for(&rx, 5.0, &mut seen, |e| *e == PlayerEvent::Ended);
    let cap = stats.capture.lock().unwrap().take().unwrap();
    let peak = cap.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!((peak - 0.5 * 0.125).abs() < 0.002, "peak {peak}");
}

#[test]
fn plays_http_stream() {
    let dir = temp_dir("p-http");
    let f = make_set(&dir, "o", 3.0, onset(1.0), &["m4a"]).remove(0);
    let srv = serve(dir, true);
    let backend = NullBackend::with_speed(2.0);
    let stats = backend.stats();
    let (p, rx) = player(backend, Duration::from_secs(30));
    let mut seen = vec![];
    p.load(Source::url(format!("{}/{}", srv.base, f.file_name().unwrap().to_str().unwrap())), true);
    wait_for(&rx, 5.0, &mut seen, |e| matches!(e, PlayerEvent::Loaded { .. }));
    p.seek(2.0);
    wait_for(&rx, 5.0, &mut seen, |e| *e == PlayerEvent::Ended);
    let secs = stats.samples.load(Ordering::SeqCst) as f64 / 2.0 / RATE as f64;
    assert!(secs > 0.9 && secs < 2.1, "rendered {secs}s");
    assert!(!seen.iter().any(|(_, e)| matches!(e, PlayerEvent::Error(_))));
}
