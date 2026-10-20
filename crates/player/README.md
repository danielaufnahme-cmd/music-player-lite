# geonix-player

Audio engine for Music Player Lite: symphonia decode (mp3, aac/m4a, flac, wav, ogg/vorbis)
+ cpal output. Local files and seekable HTTP(S) URLs (Range requests).
All `Player` methods are **non-blocking** (they post a command to the engine
thread) and safe to call from the UI thread. The queue lives in the UI; the
player only knows "current" and an optional preloaded "next" track.

```rust
use geonix_player::{Player, PlayerEvent, PlaybackState, Source, MediaMeta, MediaKey};

let (player, events) = Player::new();          // default: cpal output + OS media controls
player.load(Source::url("http://127.0.0.1:7902/api/tracks/ID/stream"), true); // autoplay
player.preload(Some(Source::url(".../NEXT/stream")));  // near-gapless hand-over; None clears
player.toggle(); player.play(); player.pause(); player.stop();
player.seek(42.0);                              // seconds
player.set_volume(0.8);                         // 0.0..=1.0, cubic curve like mpv (Flutter used media_kit volume 80)
player.set_metadata(MediaMeta { title, artist, album, cover_url, duration }); // OS now-playing
let pos: f64 = player.position();               // seconds, cheap (atomics)
let dur: Option<f64> = player.duration();
let st: PlaybackState = player.state();         // Idle | Loading | Playing | Paused | Stopped
// Idle = nothing loaded (start, after stop()); Stopped = track ended or failed
let vol: f32 = player.volume();

// events: std::sync::mpsc::Receiver<PlayerEvent>; recv on a helper thread and
// forward with slint::invoke_from_event_loop.
match events.recv()? {
    PlayerEvent::State(PlaybackState),          // every state change
    PlayerEvent::Loaded { duration: Option<f64> },     // current track opened (after load())
    PlayerEvent::Position { position: f64, duration: Option<f64> }, // <= 4 Hz while Playing; once after seek/pause/load
    PlayerEvent::AdvancedToPreload { duration: Option<f64> }, // current ended, preloaded track now playing
    PlayerEvent::Ended,                         // current ended and nothing was preloaded (state -> Stopped)
    PlayerEvent::Error(String),                 // load/decode/network error (state -> Stopped)
    PlayerEvent::MediaKey(MediaKey),            // OS media keys the UI must handle: Next | Previous | Raise | Quit
}
```

Behaviour
- Play/Pause/Toggle/Stop/Seek from OS media keys are applied by the player itself
  (you get `State`/`Position` events); only `Next`/`Previous` are forwarded.
- `Ended`: the track finished and there was no preload; position stays at the end.
  For "repeat one", call `seek(0.0); play()`; for next, call `load(next, true)`.
- `AdvancedToPreload`: the UI should advance its queue index and preload the next one.
  The preload is dropped by any `load()`.
- After `stop()` or ~30 s paused/ended the output device is released; no thread
  wakes while idle (0 % CPU). `play()` re-opens it transparently.
- `seek()` while Paused keeps it paused; position updates immediately. `play()` after `Ended`
  restarts the track from 0.
- `Player` is `Clone + Send + Sync`; the engine thread exits when the last clone drops.
- A mid-stream error (e.g. network gone) emits `Error` and then ends the track (`Ended`), so the
  UI's normal "next track" path continues.
- The waves in the Flutter app are not audio-reactive (they follow playing/paused only), so
  there is no level meter.
- Windows: SMTC needs an HWND; currently media controls are skipped there (None hwnd).

Advanced / tests
- `Player::with_options(PlayerOptions { backend: Box<dyn OutputBackend>, media_controls: bool,
  idle_release: Duration, .. })`. `NullBackend` renders silence to nowhere (real-time or
  fast), `NullBackend::stats()` (open_streams, opened_total, samples, capture) lets tests check the device was released.
- Cargo feature `media-controls` (default on) = souvlaki (MPRIS / macOS / SMTC).
