//! HTTP Range reader against a local std-TcpListener server.

mod common;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;

use common::*;
use geonix_player::decode::TrackDecoder;
use geonix_player::{HttpReader, Source};

#[test]
fn range_reader_reads_and_seeks_bytes() {
    let dir = temp_dir("http-bytes");
    let data: Vec<u8> = (0..5_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("blob.bin"), &data).unwrap();
    let srv = serve(dir, true);
    let mut r = HttpReader::open(&format!("{}/blob.bin", srv.base)).unwrap();
    assert_eq!(r.len(), Some(data.len() as u64));

    let mut all = Vec::new();
    r.read_to_end(&mut all).unwrap();
    assert!(all == data, "full read mismatch");
    // 5 MB in 2 MiB chunks = 3 range requests.
    assert_eq!(srv.range_requests.load(Ordering::SeqCst), 3);

    for pos in [4_999_000u64, 10, 2_097_150, 3_000_000] {
        r.seek(SeekFrom::Start(pos)).unwrap();
        let mut buf = [0u8; 4096];
        let n = r.read(&mut buf).unwrap();
        assert!(n > 0);
        assert_eq!(&buf[..n], &data[pos as usize..pos as usize + n], "at {pos}");
    }
    assert_eq!(r.seek(SeekFrom::End(-1)).unwrap(), data.len() as u64 - 1);
    let mut b = [0u8; 10];
    assert_eq!(r.read(&mut b).unwrap(), 1);
    assert_eq!(r.read(&mut b).unwrap(), 0);
}

#[test]
fn decodes_over_http_with_seek() {
    let dir = temp_dir("http-audio");
    // m4a from ffmpeg has its moov atom at the END: needs real Range seeking.
    let files = make_set(&dir, "onset", 8.0, onset(5.0), &["mp3", "m4a", "flac"]);
    let srv = serve(dir.clone(), true);
    for f in &files {
        let name = f.file_name().unwrap().to_str().unwrap();
        // A query string must not confuse the extension hint.
        let url = format!("{}/{name}?x=1", srv.base);
        let local: usize = {
            let mut d = TrackDecoder::open(&Source::file(f)).unwrap();
            let mut v = Vec::new();
            while d.next_chunk(&mut v).unwrap() {}
            v.len()
        };
        let mut d = TrackDecoder::open(&Source::url(&url)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut v = Vec::new();
        while d.next_chunk(&mut v).unwrap() {}
        assert_eq!(v.len(), local, "{name}: http decode differs from local");

        let mut d = TrackDecoder::open(&Source::url(&url)).unwrap();
        let got = d.seek(4.9).unwrap();
        let mut buf = Vec::new();
        while buf.len() < RATE as usize && d.next_chunk(&mut buf).unwrap() {}
        let first = buf.iter().position(|s| s.abs() > 0.1).unwrap() / 2;
        let at = got + first as f64 / RATE as f64;
        assert!((at - 5.0).abs() < 0.03, "{name}: tone at {at}");
    }
    assert!(srv.range_requests.load(Ordering::SeqCst) >= files.len() * 2);
}

#[test]
fn server_without_range_support_still_plays() {
    let dir = temp_dir("http-norange");
    let files = make_set(&dir, "sine", 3.0, sine(440.0, 0.5), &["mp3"]);
    let srv = serve(dir, false);
    let url = format!("{}/{}", srv.base, files[0].file_name().unwrap().to_str().unwrap());
    let mut d = TrackDecoder::open(&Source::url(&url)).unwrap();
    let mut v = Vec::new();
    while d.next_chunk(&mut v).unwrap() {}
    let secs = v.len() as f64 / 2.0 / RATE as f64;
    assert!((secs - 3.0).abs() < 0.05, "{secs}");
}

#[test]
fn http_errors_are_reported() {
    let dir = temp_dir("http-404");
    let srv = serve(dir, true);
    let e = TrackDecoder::open(&Source::url(format!("{}/nope.mp3", srv.base))).err().unwrap();
    assert!(e.contains("404"), "{e}");
    assert!(TrackDecoder::open(&Source::url("http://127.0.0.1:1/x.mp3")).is_err());
}
