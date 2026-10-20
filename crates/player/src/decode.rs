//! symphonia-based decoder producing interleaved f32 at the file's native
//! rate / channel count.

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymErr;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

use crate::Source;

/// One opened track.
pub struct TrackDecoder {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    pub rate: u32,
    pub channels: usize,
    pub duration: Option<f64>,
    sbuf: Option<SampleBuffer<f32>>,
    /// Interleaved samples decoded during `open` (to learn the format) not yet handed out.
    pending: Vec<f32>,
    /// Frames to drop after an accurate seek landed early.
    skip_frames: u64,
}

impl TrackDecoder {
    pub fn open(src: &Source) -> Result<Self, String> {
        let (media, ext) = src.open()?;
        let mss = MediaSourceStream::new(media, MediaSourceStreamOptions { buffer_len: 64 * 1024 });
        let mut hint = Hint::new();
        if let Some(e) = &ext {
            hint.with_extension(e);
        }
        let fmt_opts = FormatOptions { enable_gapless: true, ..Default::default() };
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &fmt_opts, &MetadataOptions::default())
            .map_err(|e| format!("unsupported or unreadable audio: {e}"))?;
        // Drop any probe-level metadata (ID3 cover art etc.) right away.
        let reader = probed.format;
        let track = reader.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio track")?;
        let params = track.codec_params.clone();
        let track_id = track.id;
        let decoder =
            symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).map_err(|e| format!("unsupported codec: {e}"))?;
        let time_base = params.time_base;
        let duration = match (params.n_frames, time_base, params.sample_rate) {
            (Some(n), Some(tb), _) => {
                let t = tb.calc_time(n);
                Some(t.seconds as f64 + t.frac)
            }
            (Some(n), None, Some(r)) => Some(n as f64 / r as f64),
            _ => None,
        };
        let mut d = TrackDecoder {
            reader,
            decoder,
            track_id,
            time_base,
            rate: params.sample_rate.unwrap_or(0),
            channels: params.channels.map(|c| c.count()).unwrap_or(0),
            duration,
            sbuf: None,
            pending: Vec::new(),
            skip_frames: 0,
        };
        // Decode the first packet: validates the stream and fills in the real
        // rate / channel count (AAC often only knows them after decoding).
        let mut first = Vec::new();
        if !d.decode_packet(&mut first)? {
            return Err("audio stream is empty".into());
        }
        d.pending = first;
        if d.rate == 0 || d.channels == 0 {
            return Err("unknown sample format".into());
        }
        Ok(d)
    }

    /// Appends the next chunk of interleaved samples to `out`.
    /// Returns `Ok(false)` at end of stream.
    pub fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        if !self.pending.is_empty() {
            out.append(&mut self.pending);
            return Ok(true);
        }
        self.decode_packet(out)
    }

    fn decode_packet(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        let mut bad = 0;
        loop {
            let packet = match self.reader.next_packet() {
                Ok(p) => p,
                Err(SymErr::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
                Err(SymErr::ResetRequired) => return Ok(false),
                Err(e) => return Err(format!("read error: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let spec = *buf.spec();
                    if buf.frames() == 0 {
                        continue;
                    }
                    self.rate = spec.rate;
                    self.channels = spec.channels.count();
                    let need = buf.capacity() as u64;
                    if self.sbuf.as_ref().is_none_or(|s| s.capacity() < buf.capacity() * self.channels) {
                        self.sbuf = Some(SampleBuffer::new(need, spec));
                    }
                    let sb = self.sbuf.as_mut().unwrap();
                    sb.copy_interleaved_ref(buf);
                    let mut s = sb.samples();
                    if self.skip_frames > 0 {
                        let frames = (s.len() / self.channels) as u64;
                        let drop = self.skip_frames.min(frames);
                        self.skip_frames -= drop;
                        s = &s[drop as usize * self.channels..];
                        if s.is_empty() {
                            continue;
                        }
                    }
                    out.extend_from_slice(s);
                    return Ok(true);
                }
                Err(SymErr::DecodeError(_)) | Err(SymErr::IoError(_)) => {
                    bad += 1;
                    if bad > 100 {
                        return Err("too many corrupt packets".into());
                    }
                }
                Err(e) => return Err(format!("decode error: {e}")),
            }
        }
    }

    /// Sample-accurate seek. Returns the position actually reached (seconds).
    pub fn seek(&mut self, secs: f64) -> Result<f64, String> {
        let secs = secs.max(0.0);
        self.pending.clear();
        self.skip_frames = 0;
        let seeked = self
            .reader
            .seek(SeekMode::Accurate, SeekTo::Time { time: Time::from(secs), track_id: Some(self.track_id) })
            .map_err(|e| format!("seek failed: {e}"))?;
        self.decoder.reset();
        let (req, act) = (seeked.required_ts, seeked.actual_ts);
        let to_secs = |ts: u64| -> f64 {
            match self.time_base {
                Some(tb) => {
                    let t = tb.calc_time(ts);
                    t.seconds as f64 + t.frac
                }
                None => ts as f64 / self.rate.max(1) as f64,
            }
        };
        if req > act {
            let gap = to_secs(req) - to_secs(act);
            self.skip_frames = (gap * self.rate as f64).round() as u64;
        }
        Ok(to_secs(req.max(act)))
    }
}
