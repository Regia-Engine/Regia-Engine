// SPDX-License-Identifier: MPL-2.0
//! `light` noise suppression for voice chat (`docs/specs/59-voice-chat.md` § 2.7 step 2,
//! ADR-2608).
//!
//! A short-time spectral gain: 20 ms windows (square-root Hann, 50 % overlap, so analysis times
//! synthesis sums to one and an all-pass gain reconstructs the input exactly, delayed by one
//! 10 ms hop). Per bin:
//!
//! - **Noise power** by minimum statistics (R. Martin, "Noise Power Spectral Density Estimation
//!   Based on Optimal Smoothing and Minimum Statistics", IEEE TSAP 2001), simplified: a fixed
//!   smoothing factor instead of Martin's optimal one, the minimum over [`NS_MIN_WINDOWS`]
//!   sub-windows of [`NS_SUBWINDOW_HOPS`] hops (about 1.3 s), and a constant bias correction
//!   ([`NS_BIAS`], calibrated on white noise so the estimate meets the true power within 1 dB).
//! - **Gain** by the Wiener rule on a decision-directed a-priori SNR (Y. Ephraim and D. Malah,
//!   "Speech enhancement using a minimum mean-square error short-time spectral amplitude
//!   estimator", IEEE TASSP 1984), floored at [`NS_GAIN_FLOOR_DB`] so the residual noise stays
//!   natural instead of "musical".
//!
//! `strong` (a recurrent network, feature `voice-denoise-rnn`) is not built; the session runs
//! this stage for it and reports `noise_suppression_light`.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Lowest gain of a bin, dB (the most `light` removes).
pub const NS_GAIN_FLOOR_DB: f32 = -12.0;
/// Hops per minimum-search sub-window.
pub const NS_SUBWINDOW_HOPS: u32 = 16;
/// Sub-windows the minimum is taken over (8 x 16 hops x 10 ms = 1.28 s).
pub const NS_MIN_WINDOWS: usize = 8;
/// Bias correction of the minimum (calibrated, ADR-2608).
pub const NS_BIAS: f32 = 2.5;
/// Periodogram smoothing factor.
const SMOOTHING: f32 = 0.8;
/// Decision-directed weight of the previous frame.
const DD_WEIGHT: f32 = 0.98;
/// Power floor that keeps digital silence finite.
const POWER_FLOOR: f32 = 1e-20;

/// Counters of a [`NoiseSuppressor`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NoiseSuppressorStats {
    /// Hops processed (two per 20 ms frame).
    pub hops: u64,
    /// FFTs run (a deterministic cost counter).
    pub ffts: u64,
    /// Mean estimated noise power over bins at the last hop, dB relative to full scale per bin.
    pub noise_floor_db: f32,
    /// Mean gain over bins at the last hop, dB.
    pub mean_gain_db: f32,
}

/// The `light` spectral noise suppressor.
pub struct NoiseSuppressor {
    hop: usize,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    buf: Vec<Complex<f32>>,
    input: Vec<f32>,
    overlap: Vec<f32>,
    smoothed: Vec<f32>,
    current_min: Vec<f32>,
    noise: Vec<f32>,
    window_mins: Vec<f32>,
    windows_filled: usize,
    next_window: usize,
    hops_in_window: u32,
    gain: Vec<f32>,
    prev_power: Vec<f32>,
    started: bool,
    stats: NoiseSuppressorStats,
}

impl std::fmt::Debug for NoiseSuppressor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseSuppressor")
            .field("hop", &self.hop)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl NoiseSuppressor {
    /// A suppressor for `sample_rate` (20 ms windows, 10 ms hops).
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let hop = (sample_rate / 100).max(8) as usize;
        let n = 2 * hop;
        let bins = hop + 1;
        let window: Vec<f32> = (0..n)
            .map(|i| {
                let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
                w.sqrt() as f32
            })
            .collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        let scratch_len = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
        let zero = Complex::new(0.0, 0.0);
        Self {
            hop,
            window,
            fft,
            ifft,
            scratch: vec![zero; scratch_len],
            buf: vec![zero; n],
            input: vec![0.0; n],
            overlap: vec![0.0; hop],
            smoothed: vec![0.0; bins],
            current_min: vec![0.0; bins],
            noise: vec![0.0; bins],
            window_mins: vec![f32::MAX; NS_MIN_WINDOWS * bins],
            windows_filled: 0,
            next_window: 0,
            hops_in_window: 0,
            gain: vec![1.0; bins],
            prev_power: vec![0.0; bins],
            started: false,
            stats: NoiseSuppressorStats::default(),
        }
    }

    /// Samples per hop (10 ms); the stage delays its input by this much.
    #[must_use]
    pub fn latency_samples(&self) -> usize {
        self.hop
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> NoiseSuppressorStats {
        self.stats
    }

    /// Clears the counters (the noise estimate stays).
    pub fn reset_stats(&mut self) {
        self.stats = NoiseSuppressorStats {
            noise_floor_db: self.stats.noise_floor_db,
            mean_gain_db: self.stats.mean_gain_db,
            ..NoiseSuppressorStats::default()
        };
    }

    /// Suppresses noise in `samples`, in place, whole hops at a time; a trailing partial hop
    /// passes through unchanged (voice frames are always two hops).
    pub fn process(&mut self, samples: &mut [f32]) {
        let hop = self.hop;
        let mut start = 0;
        while start + hop <= samples.len() {
            self.process_hop(&mut samples[start..start + hop]);
            start += hop;
        }
    }

    fn process_hop(&mut self, io: &mut [f32]) {
        let hop = self.hop;
        let n = 2 * hop;
        let bins = hop + 1;
        self.stats.hops += 1;
        self.input.copy_within(hop.., 0);
        self.input[hop..].copy_from_slice(io);
        for ((c, x), w) in self.buf.iter_mut().zip(&self.input).zip(&self.window) {
            *c = Complex::new(x * w, 0.0);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        self.stats.ffts += 1;

        // Noise tracking by minimum statistics over the smoothed periodogram.
        let first = !self.started;
        self.started = true;
        for (k, s) in self.smoothed.iter_mut().enumerate() {
            let p = self.buf[k].norm_sqr();
            *s = if first {
                p
            } else {
                SMOOTHING * *s + (1.0 - SMOOTHING) * p
            };
        }
        if self.hops_in_window == 0 {
            self.current_min.copy_from_slice(&self.smoothed);
        } else {
            for (m, s) in self.current_min.iter_mut().zip(&self.smoothed) {
                *m = m.min(*s);
            }
        }
        self.hops_in_window += 1;
        if self.hops_in_window == NS_SUBWINDOW_HOPS {
            let slot = self.next_window;
            self.window_mins[slot * bins..(slot + 1) * bins].copy_from_slice(&self.current_min);
            self.next_window = (slot + 1) % NS_MIN_WINDOWS;
            self.windows_filled = (self.windows_filled + 1).min(NS_MIN_WINDOWS);
            self.hops_in_window = 0;
        }

        self.noise.copy_from_slice(&self.current_min);
        for w in 0..self.windows_filled {
            let mins = &self.window_mins[w * bins..(w + 1) * bins];
            for (m, wm) in self.noise.iter_mut().zip(mins) {
                *m = m.min(*wm);
            }
        }
        let floor = 10f32.powf(NS_GAIN_FLOOR_DB / 20.0);
        let mut noise_sum = 0.0f64;
        let mut gain_sum = 0.0f64;
        for (((c, g), prev), noise) in self.buf[..bins]
            .iter()
            .zip(&mut self.gain)
            .zip(&mut self.prev_power)
            .zip(&self.noise)
        {
            let noise = (NS_BIAS * noise).max(POWER_FLOOR);
            let power = c.norm_sqr();
            let post = power / noise;
            let prior = DD_WEIGHT * *g * *g * *prev / noise + (1.0 - DD_WEIGHT) * (post - 1.0).max(0.0);
            let gain = (prior / (1.0 + prior)).max(floor);
            *g = if gain.is_finite() { gain } else { 1.0 };
            *prev = power;
            noise_sum += f64::from(noise);
            gain_sum += f64::from(*g);
        }
        self.stats.noise_floor_db =
            (10.0 * (noise_sum / bins as f64 / (n * n) as f64).max(1e-20).log10()) as f32;
        self.stats.mean_gain_db = (20.0 * (gain_sum / bins as f64).max(1e-10).log10()) as f32;

        // Apply the gains to the full (conjugate-symmetric) spectrum and synthesize: bins 0..=hop
        // directly, and bin n-k (k in 1..hop) with the gain of bin k.
        let (low, high) = self.buf.split_at_mut(bins);
        for (c, g) in low.iter_mut().zip(&self.gain) {
            *c *= *g;
        }
        for (c, g) in high.iter_mut().rev().zip(&self.gain[1..hop]) {
            *c *= *g;
        }
        self.ifft.process_with_scratch(&mut self.buf, &mut self.scratch);
        self.stats.ffts += 1;
        let scale = 1.0 / n as f32;
        let (first, second) = self.buf.split_at(hop);
        let (w_first, w_second) = self.window.split_at(hop);
        for (((out, o), c), w) in io.iter_mut().zip(&self.overlap).zip(first).zip(w_first) {
            *out = o + c.re * scale * w;
        }
        for ((o, c), w) in self.overlap.iter_mut().zip(second).zip(w_second) {
            *o = c.re * scale * w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_chat::dsp_fixtures::{energy, rms_db, speech_like, white_noise};

    fn run(ns: &mut NoiseSuppressor, input: &[f32], rate: u32) -> Vec<f32> {
        let frame = (rate / 50) as usize;
        let mut out = input.to_vec();
        for f in out.chunks_mut(frame) {
            ns.process(f);
        }
        // Undo the one-hop delay so samples line up with the input.
        let hop = ns.latency_samples();
        out.drain(..hop);
        out.extend(std::iter::repeat_n(0.0, hop));
        out
    }

    /// Stationary noise alone is attenuated by at least 9 dB once the tracker has settled
    /// (the gain floor allows 12), and the noise estimate meets the true per-bin power within
    /// 2 dB.
    #[test]
    fn stationary_noise_is_attenuated() {
        for rate in [16_000u32, 48_000] {
            let n = rate as usize * 5;
            let noise = white_noise(n, 5, -40.0);
            let mut ns = NoiseSuppressor::new(rate);
            let out = run(&mut ns, &noise, rate);
            let from = 2 * rate as usize;
            let to = n - rate as usize / 10;
            let atten = rms_db(&out[from..to]) - rms_db(&noise[from..to]);
            assert!(atten <= -9.0, "{rate} Hz: noise attenuation {atten:.1} dB");
            // Per-bin power of white noise through the window: N/2 * sigma^2, normalized by N^2.
            let window_n = 2.0 * f64::from(rate / 100);
            let truth = 10.0 * (window_n / 2.0 * 1e-4 / (window_n * window_n)).log10();
            let est = f64::from(ns.stats().noise_floor_db);
            assert!(
                (est - truth).abs() <= 2.0,
                "{rate} Hz: noise estimate {est:.1} dB, truth {truth:.1} dB"
            );
        }
    }

    /// Speech-like bursts over noise: the bursts keep their energy within 3 dB and the pauses
    /// lose at least 9 dB of noise.
    #[test]
    fn speech_is_kept_and_pauses_are_cleaned() {
        let rate = 16_000u32;
        let n = rate as usize * 6;
        let speech = speech_like(n, rate, 3, -20.0);
        let noise = white_noise(n, 5, -40.0);
        let mixed: Vec<f32> = speech.iter().zip(&noise).map(|(s, v)| s + v).collect();
        let mut ns = NoiseSuppressor::new(rate);
        let out = run(&mut ns, &mixed, rate);
        // Classify 20 ms frames from 2 s on by the clean speech level.
        let frame = (rate / 50) as usize;
        let (mut sp_in, mut sp_out, mut pa_in, mut pa_out) = (0.0, 0.0, 0.0, 0.0);
        for i in (2 * rate as usize / frame)..(n / frame - 1) {
            let r = i * frame..(i + 1) * frame;
            let level = rms_db(&speech[r.clone()]);
            if level > -22.0 {
                sp_in += energy(&mixed[r.clone()]);
                sp_out += energy(&out[r]);
            } else if level < -80.0 {
                pa_in += energy(&mixed[r.clone()]);
                pa_out += energy(&out[r]);
            }
        }
        assert!(sp_in > 0.0 && pa_in > 0.0, "fixture has no speech or no pauses");
        let kept = 10.0 * (sp_out / sp_in).log10();
        let cleaned = 10.0 * (pa_out / pa_in).log10();
        assert!(kept >= -3.0, "speech kept {kept:.1} dB");
        assert!(cleaned <= -9.0, "pause noise {cleaned:.1} dB");
    }

    /// Digital silence stays silent and finite.
    #[test]
    fn silence_stays_silent() {
        let mut ns = NoiseSuppressor::new(24_000);
        let mut frame = vec![0.0f32; 480];
        for _ in 0..50 {
            ns.process(&mut frame);
            assert!(frame.iter().all(|s| *s == 0.0));
        }
    }

    /// Two FFTs per 10 ms hop: four per 20 ms frame.
    #[test]
    fn cost_counters_per_frame() {
        let mut ns = NoiseSuppressor::new(48_000);
        let mut frame = white_noise(960, 1, -30.0);
        for _ in 0..10 {
            ns.process(&mut frame);
        }
        assert_eq!(ns.stats().hops, 20);
        assert_eq!(ns.stats().ffts, 40);
    }
}
