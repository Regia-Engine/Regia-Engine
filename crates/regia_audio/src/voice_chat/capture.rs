// SPDX-License-Identifier: MPL-2.0
//! Capture-side conversion (`docs/specs/59-voice-chat.md` § 2.5): channel downmix, windowed-sinc
//! polyphase resampling with state across callbacks, exact 20 ms re-framing, and the 2-pole
//! high-pass of § 2.7.

use super::error::VoiceChatError;
use super::packet::VoiceQuality;
use std::collections::VecDeque;

/// Half the kernel width in output-rate input samples (taps on each side at ratio 1).
const HALF_TAPS: f64 = 16.0;
/// Fractional-position table rows (linear interpolation between rows).
const PHASES: usize = 128;

/// Averages interleaved channels to mono, appending to `out`.
pub fn downmix_to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend_from_slice(interleaved),
        n => {
            let inv = 1.0 / n as f32;
            out.extend(interleaved.chunks_exact(n).map(|c| c.iter().sum::<f32>() * inv));
        }
    }
}

/// Streaming windowed-sinc resampler between two fixed rates. Output sample `m` sits at the
/// exact input position `m * in_rate / out_rate`, computed in integers, so the stream does not
/// drift. Windowed-sinc interpolation: J. O. Smith, "Digital Audio Resampling Home Page".
#[derive(Clone, Debug)]
pub struct Resampler {
    in_rate: u64,
    out_rate: u64,
    passthrough: bool,
    taps: usize,
    table: Vec<f32>,
    buf: Vec<f32>,
    buf_start: i64,
    next_out: u64,
}

impl Resampler {
    /// A resampler from `in_rate` to `out_rate` Hz.
    ///
    /// # Errors
    /// [`VoiceChatError::UnsupportedFormat`] for a zero rate or a rate above 384 kHz.
    pub fn new(in_rate: u32, out_rate: u32) -> Result<Self, VoiceChatError> {
        if in_rate == 0 || out_rate == 0 || in_rate > 384_000 || out_rate > 384_000 {
            return Err(VoiceChatError::UnsupportedFormat(format!(
                "rates {in_rate} -> {out_rate}"
            )));
        }
        let cutoff = (f64::from(out_rate) / f64::from(in_rate)).min(1.0);
        let half = (HALF_TAPS / cutoff).ceil() as usize;
        let taps = half * 2;
        let width = HALF_TAPS / cutoff;
        let mut table = vec![0.0f32; (PHASES + 1) * taps];
        for (row, chunk) in table.chunks_exact_mut(taps).enumerate() {
            let frac = row as f64 / PHASES as f64;
            let mut sum = 0.0f64;
            let mut weights = vec![0.0f64; taps];
            for (j, w) in weights.iter_mut().enumerate() {
                let d = j as f64 - (half as f64 - 1.0);
                let x = frac - d;
                let u = x / width;
                let window = if u.abs() < 1.0 {
                    0.42 + 0.5 * (std::f64::consts::PI * u).cos()
                        + 0.08 * (2.0 * std::f64::consts::PI * u).cos()
                } else {
                    0.0
                };
                let arg = std::f64::consts::PI * cutoff * x;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { arg.sin() / arg };
                *w = cutoff * sinc * window;
                sum += *w;
            }
            for (c, w) in chunk.iter_mut().zip(&weights) {
                *c = (w / sum) as f32;
            }
        }
        let lead = half - 1;
        Ok(Self {
            in_rate: u64::from(in_rate),
            out_rate: u64::from(out_rate),
            passthrough: in_rate == out_rate,
            taps,
            table,
            buf: vec![0.0; lead],
            buf_start: -(lead as i64),
            next_out: 0,
        })
    }

    /// Resamples `input`, appending to `out`. Leftover input is kept for the next call.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let half = (self.taps / 2) as i64;
        loop {
            let scaled = self.next_out * self.in_rate;
            let ti = (scaled / self.out_rate) as i64;
            let frac_num = scaled % self.out_rate;
            let first = ti - (half - 1);
            let last = ti + half;
            if last >= self.buf_start + self.buf.len() as i64 {
                break;
            }
            let base = (first - self.buf_start) as usize;
            let pos = frac_num as f64 / self.out_rate as f64 * PHASES as f64;
            let r0 = (pos as usize).min(PHASES - 1);
            let w1 = (pos - r0 as f64) as f32;
            let row0 = &self.table[r0 * self.taps..(r0 + 1) * self.taps];
            let row1 = &self.table[(r0 + 1) * self.taps..(r0 + 2) * self.taps];
            let window = &self.buf[base..base + self.taps];
            let mut acc = 0.0f32;
            for ((x, a), b) in window.iter().zip(row0).zip(row1) {
                acc += x * (a + (b - a) * w1);
            }
            out.push(acc);
            self.next_out += 1;
        }
        let keep_from = (self.next_out * self.in_rate / self.out_rate) as i64 - (half - 1);
        let drop = (keep_from - self.buf_start).clamp(0, self.buf.len() as i64) as usize;
        self.buf.drain(..drop);
        self.buf_start += drop as i64;
    }
}

/// Second-order high-pass (RBJ cookbook biquad) removing rumble and DC, 90 Hz by default.
#[derive(Clone, Debug)]
pub struct HighPass {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl HighPass {
    /// A high-pass at `cutoff_hz` for `sample_rate`.
    #[must_use]
    pub fn new(cutoff_hz: f32, sample_rate: u32) -> Self {
        let w0 = std::f32::consts::TAU * cutoff_hz / sample_rate as f32;
        let alpha = w0.sin() / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let cos = w0.cos();
        let a0 = 1.0 + alpha;
        Self {
            b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    /// Filters `samples` in place (transposed direct form II).
    pub fn process(&mut self, samples: &mut [f32]) {
        for s in samples {
            let x = *s;
            let y = self.b[0] * x + self.z[0];
            self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
            self.z[1] = self.b[2] * x - self.a[1] * y;
            *s = y;
        }
    }
}

/// Accumulates mono samples and emits exact frames of `frame_samples`.
#[derive(Clone, Debug)]
pub struct Framer {
    frame_samples: usize,
    pending: VecDeque<f32>,
}

impl Framer {
    /// A framer for frames of `frame_samples` samples.
    #[must_use]
    pub fn new(frame_samples: usize) -> Self {
        Self {
            frame_samples: frame_samples.max(1),
            pending: VecDeque::new(),
        }
    }

    /// Adds samples.
    pub fn push(&mut self, samples: &[f32]) {
        self.pending.extend(samples.iter().copied());
    }

    /// Takes one full frame if available.
    pub fn pop_frame(&mut self) -> Option<Vec<f32>> {
        (self.pending.len() >= self.frame_samples).then(|| self.pending.drain(..self.frame_samples).collect())
    }

    /// Takes the partial leftover zero-padded to a frame (not stretched); `None` when empty.
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        if self.pending.is_empty() {
            return None;
        }
        let mut frame: Vec<f32> = self.pending.drain(..).collect();
        frame.resize(self.frame_samples, 0.0);
        Some(frame)
    }

    /// Discards the leftover (stream restart).
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Samples waiting for a full frame.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

/// Device samples in, profile-rate 20 ms mono frames out.
#[derive(Clone, Debug)]
pub struct CaptureConverter {
    channels: usize,
    resampler: Resampler,
    framer: Framer,
    mono: Vec<f32>,
    resampled: Vec<f32>,
}

impl CaptureConverter {
    /// A converter for a device at `device_rate` with `channels` interleaved channels.
    ///
    /// # Errors
    /// [`VoiceChatError::UnsupportedFormat`] for zero channels or an unusable rate.
    pub fn new(device_rate: u32, channels: usize, quality: VoiceQuality) -> Result<Self, VoiceChatError> {
        if channels == 0 {
            return Err(VoiceChatError::UnsupportedFormat("zero channels".into()));
        }
        Ok(Self {
            channels,
            resampler: Resampler::new(device_rate, quality.sample_rate())?,
            framer: Framer::new(quality.frame_samples()),
            mono: Vec::new(),
            resampled: Vec::new(),
        })
    }

    /// Feeds interleaved device samples of any chunk size.
    pub fn push(&mut self, interleaved: &[f32]) {
        self.mono.clear();
        self.resampled.clear();
        downmix_to_mono(interleaved, self.channels, &mut self.mono);
        self.resampler.process(&self.mono, &mut self.resampled);
        self.framer.push(&self.resampled);
    }

    /// Takes the next complete frame, if any.
    pub fn pop_frame(&mut self) -> Option<Vec<f32>> {
        self.framer.pop_frame()
    }

    /// Whole frames waiting.
    #[must_use]
    pub fn frames_waiting(&self) -> usize {
        self.framer.pending() / self.framer.frame_samples
    }
}
