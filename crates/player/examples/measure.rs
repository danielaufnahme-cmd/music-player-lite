//! Decode a 3-minute MP3 to the silent null sink and report RSS / CPU while
//! playing and while paused. No audible output, no media controls.
//!
//! cargo run --release -p geonix-player --example measure [-- file.mp3] [play_secs]

use std::time::{Duration, Instant};

use geonix_player::{NullBackend, PlaybackState, Player, PlayerEvent, PlayerOptions, Source};

fn proc_status(key: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0)
}

/// utime + stime of the whole process in clock ticks.
fn cpu_ticks() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = &s[s.rfind(')').map(|i| i + 2).unwrap_or(0)..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

fn cpu_pct(ticks: u64, wall: Duration) -> f64 {
    ticks as f64 / 100.0 / wall.as_secs_f64() * 100.0 // CLK_TCK = 100 on Linux
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let play_secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let path = match args.get(1).filter(|a| !a.is_empty()) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let root =
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/geonix-lite")).unwrap_or_else(std::env::temp_dir);
            let dir = root.join("player-measure");
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join("3min.mp3");
            if !p.exists() {
                let st = std::process::Command::new("ffmpeg")
                    .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
                    .arg("aevalsrc=0.3*sin(440*2*PI*t)+0.05*random(0)|0.3*sin(554*2*PI*t)+0.05*random(1):d=180:s=44100")
                    .args(["-c:a", "libmp3lame", "-b:a", "192k"])
                    .arg(&p)
                    .status()
                    .expect("ffmpeg");
                assert!(st.success());
            }
            p
        }
    };
    println!("file: {}", path.display());
    println!("baseline RSS {} KiB", proc_status("VmRSS:"));

    let mut opts = PlayerOptions::null(NullBackend::new()); // real-time pace
    opts.idle_release = Duration::from_secs(3);
    let (player, rx) = Player::with_options(opts);
    player.load(Source::file(&path), true);
    loop {
        match rx.recv_timeout(Duration::from_secs(10)).expect("no event") {
            PlayerEvent::State(PlaybackState::Playing) => break,
            PlayerEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    std::thread::sleep(Duration::from_secs(1));
    let (t0, c0) = (Instant::now(), cpu_ticks());
    let mut ticks = 0;
    let mut peak_rss = 0;
    while t0.elapsed() < Duration::from_secs(play_secs) {
        if let Ok(PlayerEvent::Position { .. }) = rx.recv_timeout(Duration::from_millis(500)) {
            ticks += 1;
        }
        peak_rss = peak_rss.max(proc_status("VmRSS:"));
    }
    let wall = t0.elapsed();
    println!(
        "PLAYING {:.0}s: RSS {} KiB (peak sampled {} KiB, VmHWM {} KiB), CPU {:.2}%, position events {:.2} Hz, pos {:.1}s",
        wall.as_secs_f64(),
        proc_status("VmRSS:"),
        peak_rss,
        proc_status("VmHWM:"),
        cpu_pct(cpu_ticks() - c0, wall),
        ticks as f64 / wall.as_secs_f64(),
        player.position()
    );

    player.pause();
    std::thread::sleep(Duration::from_secs(4)); // > idle_release: device released
    while rx.try_recv().is_ok() {}
    let (t1, c1) = (Instant::now(), cpu_ticks());
    let mut events = 0;
    while t1.elapsed() < Duration::from_secs(10) {
        if rx.recv_timeout(Duration::from_secs(1)).is_ok() {
            events += 1;
        }
    }
    let threads = std::fs::read_dir("/proc/self/task").map(|d| d.count()).unwrap_or(0);
    println!(
        "PAUSED (device released) 10s: RSS {} KiB, CPU {:.3}%, events {events}, threads {threads}",
        proc_status("VmRSS:"),
        cpu_pct(cpu_ticks() - c1, t1.elapsed())
    );
}
