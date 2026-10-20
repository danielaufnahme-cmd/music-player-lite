//! The engine thread: owns decoders + output stream, reacts to commands.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::convert::Converter;
use crate::decode::TrackDecoder;
use crate::media::Media;
use crate::output::{OutputBackend, OutputStream, Renderer, Shared, StreamSpec};
use crate::{Cmd, PlaybackState, PlayerEvent, PlayerOptions, Source, Status};

/// Decoded audio kept ahead of the device.
const BUFFER_SECS: f64 = 0.75;
/// Engine wake-up interval while playing.
const LOOP: Duration = Duration::from_millis(100);
/// Position event interval (4 Hz).
const TICK: Duration = Duration::from_millis(250);

pub(crate) struct Engine {
    backend: Box<dyn OutputBackend>,
    stream: Option<Box<dyn OutputStream>>,
    out: Option<StreamSpec>,
    idle_release: Duration,
    cmd_tx: Sender<Cmd>,
    events: Sender<PlayerEvent>,
    status: Arc<Status>,
    shared: Arc<Shared>,
    media: Option<Media>,

    state: PlaybackState,
    cur: Option<Box<TrackDecoder>>,
    cur_eof: bool,
    ended: bool,
    dur: Option<f64>,
    next: Option<Box<TrackDecoder>>,
    /// Gapless hand-overs not yet reached by the device: (sample counter, duration).
    boundaries: VecDeque<(u64, Option<f64>)>,
    conv: Converter,
    load_gen: u64,
    preload_gen: u64,
    autoplay: bool,
    paused_since: Option<Instant>,
    last_tick: Instant,
    chunk: Vec<f32>,
    conv_buf: Vec<f32>,
}

impl Engine {
    pub fn new(opts: PlayerOptions, cmd_tx: Sender<Cmd>, events: Sender<PlayerEvent>, status: Arc<Status>, shared: Arc<Shared>) -> Self {
        let media = if opts.media_controls { Media::new(&opts.display_name, &opts.dbus_name, cmd_tx.clone()) } else { None };
        Engine {
            backend: opts.backend,
            stream: None,
            out: None,
            idle_release: opts.idle_release,
            cmd_tx,
            events,
            status,
            shared,
            media,
            state: PlaybackState::Idle,
            cur: None,
            cur_eof: false,
            ended: false,
            dur: None,
            next: None,
            boundaries: VecDeque::new(),
            conv: Converter::new(44100, 2, 44100, 2),
            load_gen: 0,
            preload_gen: 0,
            autoplay: false,
            paused_since: None,
            last_tick: Instant::now(),
            chunk: Vec::new(),
            conv_buf: Vec::new(),
        }
    }

    pub fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let cmd = match self.wait_timeout() {
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
                Some(t) => match rx.recv_timeout(t) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            if let Some(c) = cmd {
                if !self.handle(c) {
                    return;
                }
                while let Ok(c) = rx.try_recv() {
                    if !self.handle(c) {
                        return;
                    }
                }
            }
            self.service();
        }
    }

    /// How long to sleep for the next command. `None` = block (idle, 0 % CPU).
    fn wait_timeout(&self) -> Option<Duration> {
        if self.state == PlaybackState::Playing {
            return Some(LOOP);
        }
        if self.stream.is_some() {
            let since = self.paused_since.map(|t| t.elapsed()).unwrap_or_default();
            return Some(self.idle_release.saturating_sub(since));
        }
        None
    }

    fn emit(&self, e: PlayerEvent) {
        let _ = self.events.send(e);
    }

    fn consumed(&self) -> u64 {
        self.shared.consumed.load(Ordering::Acquire)
    }

    fn position(&self) -> f64 {
        self.status.position(self.consumed())
    }

    /// Position is `pos` at the current device sample counter.
    fn rebase(&self, pos: f64) {
        self.status.base.store(pos.to_bits(), Ordering::Release);
        self.status.seg_start.store(self.consumed(), Ordering::Release);
    }

    fn set_dur(&mut self, d: Option<f64>) {
        self.dur = d;
        self.status.dur.store(d.unwrap_or(f64::NAN).to_bits(), Ordering::Release);
    }

    fn set_state(&mut self, s: PlaybackState) {
        if self.state == s {
            return;
        }
        self.state = s;
        self.status.state.store(s as u8, Ordering::Release);
        self.emit(PlayerEvent::State(s));
        let pos = self.position();
        if let Some(m) = self.media.as_mut() {
            m.set_state(s, pos);
        }
    }

    fn emit_position(&mut self) {
        self.last_tick = Instant::now();
        self.emit(PlayerEvent::Position { position: self.position(), duration: self.dur });
    }

    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Shutdown => {
                self.stream = None;
                self.media = None;
                return false;
            }
            Cmd::Load(src, autoplay) => self.load(src, autoplay),
            Cmd::Preload(src) => self.preload(src),
            Cmd::Play => self.play(),
            Cmd::Pause => self.pause(),
            Cmd::Toggle => {
                if self.state == PlaybackState::Playing || (self.state == PlaybackState::Loading && self.autoplay) {
                    self.pause()
                } else {
                    self.play()
                }
            }
            Cmd::Stop => self.stop(),
            Cmd::Seek(s) => self.seek(s),
            Cmd::SeekBy(d) => {
                let p = self.position();
                self.seek(p + d)
            }
            Cmd::Volume(v) => {
                let gain = v * v * v;
                self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
            }
            Cmd::Meta(m) => {
                if let Some(media) = self.media.as_mut() {
                    media.set_meta(&m);
                    media.set_state(self.state, self.status.position(self.shared.consumed.load(Ordering::Acquire)));
                }
            }
            Cmd::Key(k) => self.emit(PlayerEvent::MediaKey(k)),
            Cmd::Opened { gen, preload, result } => self.opened(gen, preload, result),
        }
        true
    }

    fn spawn_open(&self, src: Source, gen: u64, preload: bool) {
        let tx = self.cmd_tx.clone();
        let _ = std::thread::Builder::new().name("player-open".into()).spawn(move || {
            let result = TrackDecoder::open(&src).map(Box::new);
            let _ = tx.send(Cmd::Opened { gen, preload, result });
        });
    }

    fn reset_track(&mut self) {
        self.shared.clear();
        self.cur = None;
        self.next = None;
        self.boundaries.clear();
        self.cur_eof = false;
        self.ended = false;
        self.set_dur(None);
        self.rebase(0.0);
    }

    fn load(&mut self, src: Source, autoplay: bool) {
        self.load_gen += 1;
        self.preload_gen += 1;
        if let Some(s) = self.stream.as_mut() {
            let _ = s.pause();
        }
        self.reset_track();
        self.autoplay = autoplay;
        self.paused_since = Some(Instant::now());
        self.set_state(PlaybackState::Loading);
        self.spawn_open(src, self.load_gen, false);
    }

    fn preload(&mut self, src: Option<Source>) {
        self.preload_gen += 1;
        self.next = None;
        if let Some(src) = src {
            self.spawn_open(src, self.preload_gen, true);
        }
    }

    fn opened(&mut self, gen: u64, preload: bool, result: Result<Box<TrackDecoder>, String>) {
        if preload {
            if gen == self.preload_gen {
                match result {
                    Ok(d) => self.next = Some(d),
                    // A broken next track is reported when it would play; the UI
                    // then gets Ended and loads it normally (and sees the error).
                    Err(_) => self.next = None,
                }
            }
            return;
        }
        if gen != self.load_gen {
            return;
        }
        match result {
            Ok(dec) => {
                if let Some(out) = self.out {
                    if out.rate != dec.rate || out.channels as usize != dec.channels.min(2) {
                        // Re-open at the new track's native format (no resampling).
                        self.stream = None;
                        self.out = None;
                    } else {
                        self.conv = Converter::new(dec.rate, dec.channels, out.rate, out.channels as usize);
                    }
                }
                self.set_dur(dec.duration);
                self.cur = Some(dec);
                self.cur_eof = false;
                self.ended = false;
                self.rebase(0.0);
                self.emit(PlayerEvent::Loaded { duration: self.dur });
                self.set_state(PlaybackState::Paused);
                self.paused_since = Some(Instant::now());
                if self.autoplay {
                    self.play();
                } else {
                    self.emit_position();
                }
            }
            Err(e) => {
                self.paused_since = Some(Instant::now());
                self.set_state(PlaybackState::Stopped);
                self.emit(PlayerEvent::Error(e));
            }
        }
    }

    fn ensure_stream(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        let cur = self.cur.as_ref().ok_or("nothing loaded")?;
        let (rate, ch) = (cur.rate, cur.channels);
        self.finish_boundaries();
        let pos = self.position();
        let want = StreamSpec { rate, channels: ch.min(2) as u16 };
        let (spec, stream) = self.backend.open(want, Renderer(self.shared.clone()))?;
        if self.out != Some(spec) {
            // Buffered audio is in the old format: drop it and re-seek.
            let had = self.shared.buffered() > 0;
            self.shared.clear();
            self.conv = Converter::new(rate, ch, spec.rate, spec.channels as usize);
            if had && !self.cur_eof {
                if let Some(c) = self.cur.as_mut() {
                    let _ = c.seek(pos);
                }
            }
        }
        self.out = Some(spec);
        self.status.sps.store(spec.rate as u64 * spec.channels as u64, Ordering::Release);
        self.rebase(pos);
        self.stream = Some(stream);
        Ok(())
    }

    fn play(&mut self) {
        match self.state {
            PlaybackState::Loading => {
                self.autoplay = true;
                return;
            }
            PlaybackState::Playing => return,
            _ => {}
        }
        if self.cur.is_none() {
            return;
        }
        if self.ended {
            self.seek(0.0);
        }
        if let Err(e) = self.ensure_stream() {
            self.set_state(PlaybackState::Stopped);
            self.emit(PlayerEvent::Error(format!("audio output: {e}")));
            return;
        }
        self.fill();
        if let Some(s) = self.stream.as_mut() {
            if let Err(e) = s.play() {
                self.emit(PlayerEvent::Error(format!("audio output: {e}")));
            }
        }
        self.paused_since = None;
        self.set_state(PlaybackState::Playing);
        self.emit_position();
    }

    fn pause(&mut self) {
        match self.state {
            PlaybackState::Loading => self.autoplay = false,
            PlaybackState::Playing => {
                if let Some(s) = self.stream.as_mut() {
                    let _ = s.pause();
                }
                self.paused_since = Some(Instant::now());
                self.set_state(PlaybackState::Paused);
                self.emit_position();
            }
            _ => {}
        }
    }

    fn stop(&mut self) {
        self.load_gen += 1;
        self.preload_gen += 1;
        self.stream = None;
        self.reset_track();
        self.paused_since = None;
        self.set_state(PlaybackState::Idle);
    }

    fn seek(&mut self, secs: f64) {
        if self.cur.is_none() || self.state == PlaybackState::Loading || !secs.is_finite() {
            return;
        }
        self.finish_boundaries();
        let mut target = secs.max(0.0);
        if let Some(d) = self.dur {
            target = target.min((d - 0.05).max(0.0));
        }
        self.shared.clear();
        let cur = self.cur.as_mut().unwrap();
        let (rate, ch) = (cur.rate, cur.channels);
        match cur.seek(target) {
            Ok(actual) => self.rebase(actual),
            Err(e) => {
                self.rebase(self.position());
                self.emit(PlayerEvent::Error(e));
            }
        }
        if let Some(out) = self.out {
            self.conv = Converter::new(rate, ch, out.rate, out.channels as usize);
        }
        self.cur_eof = false;
        if self.ended {
            self.ended = false;
            self.set_state(PlaybackState::Paused);
        }
        if self.state == PlaybackState::Playing {
            self.fill();
        }
        self.emit_position();
        let pos = self.position();
        if let Some(m) = self.media.as_mut() {
            m.set_state(self.state, pos);
        }
    }

    /// Apply all pending gapless hand-overs immediately.
    fn finish_boundaries(&mut self) {
        while let Some((_, d)) = self.boundaries.pop_front() {
            self.set_dur(d);
            self.rebase(0.0);
            self.emit(PlayerEvent::AdvancedToPreload { duration: d });
        }
    }

    fn switch_to_next(&mut self) {
        let Some(next) = self.next.take() else { return };
        let out = self.out.unwrap_or(StreamSpec { rate: next.rate, channels: 2 });
        self.conv = Converter::new(next.rate, next.channels, out.rate, out.channels as usize);
        self.boundaries.push_back((self.shared.written.load(Ordering::Acquire), next.duration));
        self.cur = Some(next);
        self.cur_eof = false;
    }

    fn fill(&mut self) {
        let Some(out) = self.out else { return };
        let target = (out.rate as f64 * out.channels as f64 * BUFFER_SECS) as usize;
        let mut chunk = std::mem::take(&mut self.chunk);
        let mut conv_buf = std::mem::take(&mut self.conv_buf);
        while self.shared.buffered() < target {
            if self.cur_eof {
                if self.next.is_some() {
                    self.switch_to_next();
                    continue;
                }
                break;
            }
            let Some(cur) = self.cur.as_mut() else { break };
            chunk.clear();
            match cur.next_chunk(&mut chunk) {
                Ok(true) => {
                    conv_buf.clear();
                    self.conv.process(&chunk, &mut conv_buf);
                    self.shared.push(&conv_buf);
                }
                Ok(false) => self.cur_eof = true,
                Err(e) => {
                    self.cur_eof = true;
                    self.emit(PlayerEvent::Error(e));
                }
            }
        }
        self.chunk = chunk;
        self.conv_buf = conv_buf;
    }

    fn service(&mut self) {
        if self.state == PlaybackState::Playing {
            if self.stream.as_ref().is_some_and(|s| s.failed()) {
                // Device vanished: reopen on the (new) default device.
                self.stream = None;
                self.out = None;
                match self.ensure_stream() {
                    Ok(()) => {
                        if let Some(s) = self.stream.as_mut() {
                            let _ = s.play();
                        }
                    }
                    Err(e) => {
                        self.set_state(PlaybackState::Paused);
                        self.paused_since = Some(Instant::now());
                        self.emit(PlayerEvent::Error(format!("audio output: {e}")));
                        return;
                    }
                }
            }
            self.fill();
            let consumed = self.consumed();
            while self.boundaries.front().is_some_and(|(b, _)| *b <= consumed) {
                let (b, d) = self.boundaries.pop_front().unwrap();
                self.set_dur(d);
                self.status.base.store(0f64.to_bits(), Ordering::Release);
                self.status.seg_start.store(b, Ordering::Release);
                self.emit(PlayerEvent::AdvancedToPreload { duration: d });
                self.emit_position();
                let pos = self.position();
                if let Some(m) = self.media.as_mut() {
                    m.set_state(self.state, pos);
                }
            }
            if self.cur_eof && self.next.is_none() && self.boundaries.is_empty() && consumed >= self.shared.written.load(Ordering::Acquire)
            {
                if let Some(s) = self.stream.as_mut() {
                    let _ = s.pause();
                }
                self.ended = true;
                self.paused_since = Some(Instant::now());
                if let Some(d) = self.dur {
                    self.rebase(d);
                }
                self.set_state(PlaybackState::Stopped);
                self.emit(PlayerEvent::Ended);
                return;
            }
            if self.last_tick.elapsed() >= TICK {
                self.emit_position();
            }
        } else if self.stream.is_some() && self.paused_since.is_some_and(|t| t.elapsed() >= self.idle_release) {
            // Release the device; play() reopens it. Buffered audio is kept.
            self.stream = None;
        }
    }
}
