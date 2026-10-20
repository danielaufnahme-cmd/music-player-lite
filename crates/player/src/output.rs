//! Output backends: the real one (cpal) and a silent null sink for tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Sample rate + channel count of an output stream (samples are interleaved f32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    pub rate: u32,
    pub channels: u16,
}

/// Opens output streams. Implement this to plug in another sink.
pub trait OutputBackend: Send + 'static {
    /// Opens a (paused) stream as close to `want` as possible and returns its
    /// actual spec. The stream pulls audio by calling [`Renderer::fill`].
    /// Dropping the returned handle must release the device.
    fn open(&mut self, want: StreamSpec, render: Renderer) -> Result<(StreamSpec, Box<dyn OutputStream>), String>;
}

/// A live stream. Dropping it releases the device.
pub trait OutputStream {
    fn play(&mut self) -> Result<(), String>;
    fn pause(&mut self) -> Result<(), String>;
    /// True if the device reported a fatal error (e.g. unplugged); the engine reopens.
    fn failed(&self) -> bool {
        false
    }
}

/// Buffer shared between the engine (producer) and the audio callback (consumer).
pub(crate) struct Shared {
    pub q: Mutex<VecDeque<f32>>,
    /// Samples ever pushed / popped (interleaved samples, not frames).
    pub written: AtomicU64,
    pub consumed: AtomicU64,
    /// Linear gain as f32 bits.
    pub gain: AtomicU32,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Shared {
            q: Mutex::new(VecDeque::new()),
            written: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            gain: AtomicU32::new(1.0f32.to_bits()),
        })
    }

    pub fn push(&self, s: &[f32]) {
        let mut q = self.q.lock().unwrap();
        q.extend(s.iter().copied());
        self.written.fetch_add(s.len() as u64, Ordering::Release);
    }

    pub fn clear(&self) {
        let mut q = self.q.lock().unwrap();
        q.clear();
        q.shrink_to(0);
        self.consumed.store(self.written.load(Ordering::Acquire), Ordering::Release);
    }

    pub fn buffered(&self) -> usize {
        self.q.lock().unwrap().len()
    }
}

/// Handed to an [`OutputBackend`]; the audio callback calls [`Renderer::fill`].
#[derive(Clone)]
pub struct Renderer(pub(crate) Arc<Shared>);

impl Renderer {
    /// Fills `out` (interleaved, in the stream's spec) with audio, silence on
    /// underrun. Returns how many samples were real audio.
    pub fn fill(&self, out: &mut [f32]) -> usize {
        let gain = f32::from_bits(self.0.gain.load(Ordering::Relaxed));
        let n = {
            let mut q = self.0.q.lock().unwrap();
            let n = out.len().min(q.len());
            for (o, s) in out[..n].iter_mut().zip(q.drain(..n)) {
                *o = s * gain;
            }
            self.0.consumed.fetch_add(n as u64, Ordering::Release);
            n
        };
        out[n..].fill(0.0);
        n
    }
}

// ---------------------------------------------------------------- null sink

/// Silent sink: pulls audio at real-time pace (or `speed`× faster) and throws
/// it away. Used by tests and the measurement example; never makes a sound.
pub struct NullBackend {
    rate: Option<u32>,
    speed: f64,
    stats: Arc<NullStats>,
}

#[derive(Default)]
pub struct NullStats {
    /// Currently open streams (0 after the device was released).
    pub open_streams: AtomicUsize,
    /// Total number of streams ever opened.
    pub opened_total: AtomicUsize,
    /// Real (non-silence) samples consumed.
    pub samples: AtomicU64,
    /// Optional capture of everything rendered (tests only).
    pub capture: Mutex<Option<Vec<f32>>>,
}

impl NullBackend {
    /// Real-time pace, stream at the requested rate.
    pub fn new() -> Self {
        Self::with_speed(1.0)
    }

    /// `speed` > 1 pulls audio faster than real time (tests).
    pub fn with_speed(speed: f64) -> Self {
        NullBackend { rate: None, speed, stats: Arc::new(NullStats::default()) }
    }

    /// Force the device rate (to exercise the resampler).
    pub fn with_rate(mut self, rate: u32) -> Self {
        self.rate = Some(rate);
        self
    }

    /// Record every rendered sample into `stats().capture`.
    pub fn capture(self) -> Self {
        *self.stats.capture.lock().unwrap() = Some(Vec::new());
        self
    }

    pub fn stats(&self) -> Arc<NullStats> {
        self.stats.clone()
    }
}

impl Default for NullBackend {
    fn default() -> Self {
        Self::new()
    }
}

struct NullCtl {
    playing: bool,
    quit: bool,
}

struct NullStream {
    ctl: Arc<(Mutex<NullCtl>, Condvar)>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<NullStats>,
}

impl OutputBackend for NullBackend {
    fn open(&mut self, want: StreamSpec, render: Renderer) -> Result<(StreamSpec, Box<dyn OutputStream>), String> {
        let spec = StreamSpec { rate: self.rate.unwrap_or(want.rate), channels: want.channels.clamp(1, 2) };
        let ctl = Arc::new((Mutex::new(NullCtl { playing: false, quit: false }), Condvar::new()));
        let stats = self.stats.clone();
        stats.open_streams.fetch_add(1, Ordering::SeqCst);
        stats.opened_total.fetch_add(1, Ordering::SeqCst);
        let speed = self.speed;
        let (c2, s2) = (ctl.clone(), stats.clone());
        let thread = std::thread::Builder::new()
            .name("null-sink".into())
            .spawn(move || {
                let frames = 1024usize;
                let mut buf = vec![0f32; frames * spec.channels as usize];
                let period = Duration::from_secs_f64(frames as f64 / spec.rate as f64 / speed);
                loop {
                    {
                        let (m, cv) = &*c2;
                        let mut g = m.lock().unwrap();
                        while !g.playing && !g.quit {
                            g = cv.wait(g).unwrap();
                        }
                        if g.quit {
                            return;
                        }
                    }
                    let n = render.fill(&mut buf);
                    s2.samples.fetch_add(n as u64, Ordering::Relaxed);
                    if let Some(c) = s2.capture.lock().unwrap().as_mut() {
                        c.extend_from_slice(&buf[..n]);
                    }
                    let sleep = if n == 0 { period.max(Duration::from_millis(1)) } else { period };
                    std::thread::sleep(sleep);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok((spec, Box::new(NullStream { ctl, thread: Some(thread), stats })))
    }
}

impl NullStream {
    fn set(&self, playing: bool) {
        let (m, cv) = &*self.ctl;
        m.lock().unwrap().playing = playing;
        cv.notify_all();
    }
}

impl OutputStream for NullStream {
    fn play(&mut self) -> Result<(), String> {
        self.set(true);
        Ok(())
    }
    fn pause(&mut self) -> Result<(), String> {
        self.set(false);
        Ok(())
    }
}

impl Drop for NullStream {
    fn drop(&mut self) {
        {
            let (m, cv) = &*self.ctl;
            m.lock().unwrap().quit = true;
            cv.notify_all();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.stats.open_streams.fetch_sub(1, Ordering::SeqCst);
    }
}

// --------------------------------------------------------------------- cpal

/// The system's default output device via cpal (PipeWire/Pulse through ALSA
/// on Linux, CoreAudio, WASAPI).
#[derive(Default)]
pub struct CpalBackend;

struct CpalStream {
    stream: cpal::Stream,
    failed: Arc<AtomicBool>,
}

impl OutputBackend for CpalBackend {
    fn open(&mut self, want: StreamSpec, render: Renderer) -> Result<(StreamSpec, Box<dyn OutputStream>), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use cpal::SampleFormat;

        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no audio output device")?;
        let want_ch = want.channels.clamp(1, 2);

        // Prefer the file's own rate (no resampling) with f32 samples.
        let mut best: Option<(i32, cpal::SupportedStreamConfig)> = None;
        if let Ok(cfgs) = device.supported_output_configs() {
            for c in cfgs {
                let fits_rate = c.min_sample_rate().0 <= want.rate && want.rate <= c.max_sample_rate().0;
                let rate = if fits_rate { want.rate } else { c.max_sample_rate().0.min(48000).max(c.min_sample_rate().0) };
                let score = (fits_rate as i32) * 8
                    + (c.channels() == want_ch) as i32 * 4
                    + (c.channels() == 2) as i32 * 2
                    + (c.sample_format() == SampleFormat::F32) as i32;
                if c.channels() == 0 {
                    continue;
                }
                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, c.with_sample_rate(cpal::SampleRate(rate))));
                }
            }
        }
        let supported = match best {
            Some((_, c)) => c,
            None => device.default_output_config().map_err(|e| e.to_string())?,
        };
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let spec = StreamSpec { rate: config.sample_rate.0, channels: config.channels };
        let failed = Arc::new(AtomicBool::new(false));
        let f2 = failed.clone();
        let on_err = move |e: cpal::StreamError| {
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                f2.store(true, Ordering::Relaxed);
            }
        };

        // Output may have more channels than we produce (e.g. 5.1): we render
        // `ours` channels and spread them over the device's.
        let dev_ch = config.channels as usize;
        let ours = dev_ch.min(2);
        let spec = StreamSpec { rate: spec.rate, channels: ours as u16 };

        fn build<T: cpal::SizedSample + cpal::FromSample<f32>>(
            device: &cpal::Device,
            config: &cpal::StreamConfig,
            render: Renderer,
            dev_ch: usize,
            ours: usize,
            on_err: impl FnMut(cpal::StreamError) + Send + 'static,
        ) -> Result<cpal::Stream, cpal::BuildStreamError> {
            let mut tmp: Vec<f32> = Vec::new();
            device.build_output_stream(
                config,
                move |data: &mut [T], _| {
                    let frames = data.len() / dev_ch;
                    tmp.resize(frames * ours, 0.0);
                    render.fill(&mut tmp);
                    for (f, frame) in data.chunks_mut(dev_ch).enumerate() {
                        for (c, s) in frame.iter_mut().enumerate() {
                            let v = if c < ours { tmp[f * ours + c] } else { 0.0 };
                            *s = T::from_sample(v);
                        }
                    }
                },
                on_err,
                None,
            )
        }

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, render, dev_ch, ours, on_err),
            SampleFormat::I16 => build::<i16>(&device, &config, render, dev_ch, ours, on_err),
            SampleFormat::U16 => build::<u16>(&device, &config, render, dev_ch, ours, on_err),
            SampleFormat::I32 => build::<i32>(&device, &config, render, dev_ch, ours, on_err),
            SampleFormat::F64 => build::<f64>(&device, &config, render, dev_ch, ours, on_err),
            other => return Err(format!("unsupported sample format {other:?}")),
        }
        .map_err(|e| e.to_string())?;
        // Streams may start running on creation on some hosts: keep paused until play().
        let _ = stream.pause();
        Ok((spec, Box::new(CpalStream { stream, failed })))
    }
}

impl OutputStream for CpalStream {
    fn play(&mut self) -> Result<(), String> {
        use cpal::traits::StreamTrait;
        self.stream.play().map_err(|e| e.to_string())
    }
    fn pause(&mut self) -> Result<(), String> {
        use cpal::traits::StreamTrait;
        self.stream.pause().map_err(|e| e.to_string())
    }
    fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}
