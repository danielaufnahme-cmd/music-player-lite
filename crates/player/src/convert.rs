//! Channel mapping + linear-interpolation resampling (only used when the
//! device can't run at the file's rate).

pub struct Converter {
    in_ch: usize,
    out_ch: usize,
    step: f64,
    t: f64,
    prev: Vec<f32>,
    has_prev: bool,
    frame: Vec<f32>,
}

impl Converter {
    pub fn new(in_rate: u32, in_ch: usize, out_rate: u32, out_ch: usize) -> Self {
        Converter {
            in_ch: in_ch.max(1),
            out_ch: out_ch.max(1),
            step: in_rate as f64 / out_rate.max(1) as f64,
            t: 0.0,
            prev: vec![0.0; out_ch.max(1)],
            has_prev: false,
            frame: vec![0.0; out_ch.max(1)],
        }
    }

    pub fn passthrough(&self) -> bool {
        self.in_ch == self.out_ch && self.step == 1.0
    }

    fn map(&mut self, inp: &[f32]) {
        let (i, o) = (self.in_ch, self.out_ch);
        if i == o {
            self.frame.copy_from_slice(inp);
        } else if i == 1 {
            self.frame.fill(inp[0]);
        } else if o == 1 {
            self.frame[0] = inp.iter().sum::<f32>() / i as f32;
        } else {
            // More channels than the output: keep front left/right (+ others to 0).
            for (c, f) in self.frame.iter_mut().enumerate() {
                *f = inp.get(c).copied().unwrap_or(0.0);
            }
        }
    }

    /// Converts interleaved `input` and appends to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough() {
            out.extend_from_slice(input);
            return;
        }
        let resample = self.step != 1.0;
        for f in input.chunks_exact(self.in_ch) {
            self.map(f);
            if !resample {
                out.extend_from_slice(&self.frame);
                continue;
            }
            if !self.has_prev {
                self.prev.copy_from_slice(&self.frame);
                self.has_prev = true;
                continue;
            }
            while self.t < 1.0 {
                let t = self.t as f32;
                for c in 0..self.out_ch {
                    out.push(self.prev[c] + (self.frame[c] - self.prev[c]) * t);
                }
                self.t += self.step;
            }
            self.t -= 1.0;
            self.prev.copy_from_slice(&self.frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_to_stereo() {
        let mut c = Converter::new(44100, 1, 44100, 2);
        let mut out = vec![];
        c.process(&[0.5, -0.5], &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn resample_length() {
        let mut c = Converter::new(44100, 2, 48000, 2);
        let input = vec![0.1f32; 44100 * 2];
        let mut out = vec![];
        c.process(&input, &mut out);
        let frames = out.len() / 2;
        assert!((frames as i64 - 48000).abs() <= 2, "{frames}");
    }
}
