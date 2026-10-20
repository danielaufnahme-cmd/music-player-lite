//! Decoder tests on generated files (wav, mp3, m4a, flac, ogg/vorbis).

mod common;
use common::*;
use geonix_player::decode::TrackDecoder;
use geonix_player::Source;

const EXTS: &[&str] = &["wav", "mp3", "m4a", "flac", "ogg"];

fn decode_all(d: &mut TrackDecoder) -> Vec<f32> {
    let mut all = Vec::new();
    while d.next_chunk(&mut all).unwrap() {}
    all
}

#[test]
fn decodes_every_format() {
    let dir = temp_dir("formats");
    let files = make_set(&dir, "sine", 5.0, sine(440.0, 0.5), EXTS);
    for f in files {
        let mut d = TrackDecoder::open(&Source::file(&f)).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        assert_eq!(d.rate, RATE, "{}", f.display());
        assert_eq!(d.channels, 2, "{}", f.display());
        let samples = decode_all(&mut d);
        let secs = samples.len() as f64 / 2.0 / RATE as f64;
        assert!((secs - 5.0).abs() < 0.1, "{}: decoded {secs:.3}s", f.display());
        if let Some(dur) = d.duration {
            assert!((dur - 5.0).abs() < 0.1, "{}: duration {dur:.3}", f.display());
        }
        // Real signal, not silence: RMS of a 0.5 sine is ~0.354.
        let mid = &samples[samples.len() / 4..samples.len() / 2];
        let rms = (mid.iter().map(|s| s * s).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.354).abs() < 0.05, "{}: rms {rms}", f.display());
        println!("{}: {secs:.4}s, duration {:?}", f.display(), d.duration);
    }
}

/// First frame whose amplitude exceeds 0.1.
fn first_loud(samples: &[f32]) -> usize {
    samples.iter().position(|s| s.abs() > 0.1).expect("no tone") / 2
}

#[test]
fn seek_is_accurate() {
    let dir = temp_dir("seek");
    let files = make_set(&dir, "onset", 8.0, onset(5.0), EXTS);
    for f in files {
        let lossy = !matches!(f.extension().unwrap().to_str().unwrap(), "wav" | "flac");
        let tol = if lossy { 0.03 } else { 0.0005 };
        for target in [4.7, 2.0, 4.9] {
            let mut d = TrackDecoder::open(&Source::file(&f)).unwrap();
            let got = d.seek(target).unwrap();
            assert!((got - target).abs() < 0.03, "{}: seek {target} -> {got}", f.display());
            let mut buf = Vec::new();
            while buf.len() < RATE as usize * 2 && d.next_chunk(&mut buf).unwrap() {}
            if target < 4.5 {
                // Far from the onset: should be silent for the next second.
                assert!(buf[..(RATE as usize)].iter().all(|s| s.abs() < 0.1), "{}", f.display());
                continue;
            }
            let onset_at = got + first_loud(&buf) as f64 / RATE as f64;
            assert!((onset_at - 5.0).abs() < tol, "{}: seek {target}: tone at {onset_at:.4}s", f.display());
            println!(
                "{}: seek {target} -> {got:.4}, tone heard at {onset_at:.4}s (error {:+.1} ms)",
                f.display(),
                (onset_at - 5.0) * 1000.0
            );
        }
        // Backwards after forwards on the same decoder.
        let mut d = TrackDecoder::open(&Source::file(&f)).unwrap();
        d.seek(6.0).unwrap();
        let got = d.seek(4.8).unwrap();
        let mut buf = Vec::new();
        while buf.len() < RATE as usize * 2 && d.next_chunk(&mut buf).unwrap() {}
        let onset_at = got + first_loud(&buf) as f64 / RATE as f64;
        assert!((onset_at - 5.0).abs() < tol, "{}: back-seek tone at {onset_at}", f.display());
    }
}

#[test]
fn rejects_garbage() {
    let dir = temp_dir("garbage");
    let p = dir.join("x.mp3");
    std::fs::write(&p, vec![0x55u8; 10_000]).unwrap();
    assert!(TrackDecoder::open(&Source::file(&p)).is_err());
    assert!(TrackDecoder::open(&Source::file(dir.join("missing.mp3"))).is_err());
}
