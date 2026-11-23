// SPDX-License-Identifier: MPL-2.0
//! Acoustic echo cancellation for voice chat (`docs/specs/59-voice-chat.md` § 2.7 step 3,
//! ADR-2608).
//!
//! A partitioned-block frequency-domain adaptive filter (the multidelay block frequency-domain
//! filter of Soo and Pang, 1990) with per-bin NLMS step normalization, run on 10 ms blocks with
//! 50 % overlap-save FFTs. The far-end signal (what the speakers play) is split into blocks; each
//! block's spectrum enters a history of `P` partitions, and the echo estimate is the sum over the
//! partitions of filter spectrum times history spectrum, which covers [`AEC_TAIL_MS`] of room
//! response.
//!
//! Double talk (the local player speaking while the far end plays) would make a plain NLMS filter
//! adapt on the local voice and diverge. Two guards, both from the public literature:
//!
//! - **Two echo-path models** (Ochiai, Araseki and Ogihara, IEEE Trans. Commun. 1977): a
//!   *background* filter adapts every block; a *foreground* filter produces the output and is only
//!   overwritten by the background when the background has removed clearly more echo for several
//!   blocks in a row. When the background diverges (its error far above the foreground's), it is
//!   reset to the foreground. Double talk therefore never reaches the output path.
//! - **An energy double-talk detector**: once the foreground has converged, a block whose
//!   microphone energy is far above the foreground's echo estimate is double talk, and the
//!   background step drops to 1/8 for that block.
//!
//! Adaptation also stops while the far end is silent. Every buffer is sized at construction; a
//! block allocates nothing.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Room response length the filter covers, ms.
pub const AEC_TAIL_MS: u32 = 128;
/// Background NLMS step size (normalized; tuned on the synthetic fixtures, ADR-2608).
pub const AEC_STEP: f32 = 0.7;
/// Background step multiplier during detected double talk.
pub const AEC_DOUBLE_TALK_STEP_SCALE: f32 = 0.125;
/// Far-end block energy per sample below which the filter does not adapt (-70 dBFS).
const FAR_ACTIVE_POWER: f32 = 1e-7;
/// Consecutive "background clearly better" blocks before a background-to-foreground copy.
const TRANSFER_BLOCKS: u32 = 3;
/// Background error energy below this fraction of the foreground's counts as "clearly better".
const TRANSFER_RATIO: f32 = 0.8;
/// Background error energy above this multiple of the foreground's is divergence.
const RESET_RATIO: f32 = 4.0;
/// Microphone energy above this multiple of the echo estimate is double talk (6 dB).
const DOUBLE_TALK_RATIO: f32 = 4.0;
/// Smoothed foreground ERLE (dB) above which the double-talk detector is armed.
const DOUBLE_TALK_ARMED_DB: f32 = 6.0;

/// Counters of an [`EchoCanceller`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AecStats {
    /// Blocks processed.
    pub blocks: u64,
    /// Blocks in which the background filter adapted (far end active).
    pub adapted_blocks: u64,
    /// Blocks flagged as double talk.
    pub double_talk_blocks: u64,
    /// Background-to-foreground copies.
    pub transfers: u64,
    /// Background resets after divergence.
    pub resets: u64,
    /// Smoothed echo return loss enhancement of the output path over far-end-active blocks, dB.
    pub erle_db: f32,
    /// FFTs run (forward and inverse), a deterministic cost counter.
    pub ffts: u64,
    /// Complex multiply-accumulates in filtering and adaptation, a deterministic cost counter.
    pub complex_macs: u64,
}

/// A partitioned-block frequency-domain NLMS echo canceller with a two-path double-talk guard.
pub struct EchoCanceller {
    block: usize,
    bins: usize,
    parts: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex<f32>>,
    buf: Vec<Complex<f32>>,
    x_prev: Vec<f32>,
    /// `parts` far-end spectra; `hist[(head + p) % parts]` is `p` blocks old.
    hist: Vec<Complex<f32>>,
    head: usize,
    /// Sum over partitions of |X|^2 per bin (the NLMS normalization).
    power: Vec<f32>,
    w_bg: Vec<Complex<f32>>,
    w_fg: Vec<Complex<f32>>,
    acc: Vec<Complex<f32>>,
    e_spec: Vec<Complex<f32>>,
    y_bg: Vec<f32>,
    y_fg: Vec<f32>,
    time: Vec<f32>,
    constrain_next: usize,
    see_bg: f32,
    see_fg: f32,
    good_blocks: u32,
    stats: AecStats,
}

impl std::fmt::Debug for EchoCanceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoCanceller")
            .field("block", &self.block)
            .field("parts", &self.parts)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl EchoCanceller {
    /// A canceller for `sample_rate` covering [`AEC_TAIL_MS`] with 10 ms blocks.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let block = (sample_rate / 100).max(8) as usize;
        let n = 2 * block;
        let bins = block + 1;
        let tail = (sample_rate as usize * AEC_TAIL_MS as usize).div_ceil(1000);
        let parts = tail.div_ceil(block).max(1);
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(n);
        let ifft = planner.plan_fft_inverse(n);
        let scratch_len = fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len());
        let zero = Complex::new(0.0, 0.0);
        Self {
            block,
            bins,
            parts,
            fft,
            ifft,
            scratch: vec![zero; scratch_len],
            buf: vec![zero; n],
            x_prev: vec![0.0; block],
            hist: vec![zero; parts * bins],
            head: 0,
            power: vec![0.0; bins],
            w_bg: vec![zero; parts * bins],
            w_fg: vec![zero; parts * bins],
            acc: vec![zero; bins],
            e_spec: vec![zero; bins],
            y_bg: vec![0.0; block],
            y_fg: vec![0.0; block],
            time: vec![0.0; n],
            constrain_next: 0,
            see_bg: 0.0,
            see_fg: 0.0,
            good_blocks: 0,
            stats: AecStats::default(),
        }
    }

    /// Samples per block (10 ms).
    #[must_use]
    pub fn block_samples(&self) -> usize {
        self.block
    }

    /// Filter partitions (blocks of tail).
    #[must_use]
    pub fn partitions(&self) -> usize {
        self.parts
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> AecStats {
        self.stats
    }

    /// Clears the counters (the filter keeps what it learned).
    pub fn reset_stats(&mut self) {
        let erle = self.stats.erle_db;
        self.stats = AecStats {
            erle_db: erle,
            ..AecStats::default()
        };
    }

    /// Cancels the echo of `far` in `mic`, in place. Both slices hold the same number of samples;
    /// whole blocks are processed and a trailing partial block passes through unchanged (voice
    /// frames are always two blocks).
    pub fn process(&mut self, mic: &mut [f32], far: &[f32]) {
        let n = mic.len().min(far.len());
        let b = self.block;
        let mut start = 0;
        while start + b <= n {
            self.process_block(&mut mic[start..start + b], &far[start..start + b]);
            start += b;
        }
    }

    fn forward_real(&mut self, out: Spectrum) {
        for (c, t) in self.buf.iter_mut().zip(&self.time) {
            *c = Complex::new(*t, 0.0);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        self.stats.ffts += 1;
        let bins = self.bins;
        let dst = match out {
            Spectrum::Error => &mut self.e_spec[..],
            Spectrum::History(i) => &mut self.hist[i * bins..(i + 1) * bins],
            Spectrum::Weights(i) => &mut self.w_bg[i * bins..(i + 1) * bins],
        };
        dst.copy_from_slice(&self.buf[..bins]);
    }

    /// Inverse of a half spectrum held in `acc` into `time` (scaled by 1/N).
    fn inverse_acc(&mut self) {
        let n = self.buf.len();
        let scale = 1.0 / n as f32;
        self.buf[..self.bins].copy_from_slice(&self.acc);
        // Bin n-k (k in 1..block) is the conjugate of bin k.
        for (dst, src) in self.buf[self.bins..]
            .iter_mut()
            .rev()
            .zip(&self.acc[1..self.block])
        {
            *dst = src.conj();
        }
        self.ifft.process_with_scratch(&mut self.buf, &mut self.scratch);
        self.stats.ffts += 1;
        for (t, c) in self.time.iter_mut().zip(&self.buf) {
            *t = c.re * scale;
        }
    }

    /// `acc = sum_p w[p] * hist[p]`.
    fn filter(&mut self, foreground: bool) {
        let bins = self.bins;
        self.acc.fill(Complex::new(0.0, 0.0));
        for p in 0..self.parts {
            let slot = (self.head + p) % self.parts;
            let x = &self.hist[slot * bins..(slot + 1) * bins];
            let w = if foreground {
                &self.w_fg[p * bins..(p + 1) * bins]
            } else {
                &self.w_bg[p * bins..(p + 1) * bins]
            };
            for ((a, wk), xk) in self.acc.iter_mut().zip(w).zip(x) {
                *a += wk * xk;
            }
        }
        self.stats.complex_macs += (self.parts * bins) as u64;
    }

    fn process_block(&mut self, mic: &mut [f32], far: &[f32]) {
        let b = self.block;
        let bins = self.bins;
        self.stats.blocks += 1;
        // New far-end spectrum from [previous block, this block].
        self.head = (self.head + self.parts - 1) % self.parts;
        let oldest = self.head;
        self.time[..b].copy_from_slice(&self.x_prev);
        self.time[b..].copy_from_slice(far);
        self.x_prev.copy_from_slice(far);
        self.forward_real(Spectrum::History(oldest));
        // The NLMS normalization, summed afresh every block (a running sum drifts in f32 after
        // loud passages and would stall adaptation on quiet ones).
        self.power.fill(0.0);
        for slot in self.hist.chunks_exact(bins) {
            for (p, x) in self.power.iter_mut().zip(slot) {
                *p += x.norm_sqr();
            }
        }
        // Echo estimates of both paths.
        self.filter(false);
        self.inverse_acc();
        self.y_bg.copy_from_slice(&self.time[b..]);
        self.filter(true);
        self.inverse_acc();
        self.y_fg.copy_from_slice(&self.time[b..]);

        let mut sd = 0.0f32;
        let mut seb = 0.0f32;
        let mut sef = 0.0f32;
        let mut syf = 0.0f32;
        let mut sx = 0.0f32;
        for (((m, yb), yf), x) in mic.iter_mut().zip(&mut self.y_bg).zip(&self.y_fg).zip(far) {
            let d = *m;
            let eb = d - *yb;
            let ef = d - yf;
            sd += d * d;
            seb += eb * eb;
            sef += ef * ef;
            syf += yf * yf;
            sx += x * x;
            // `y_bg` now holds the background error for the update below.
            *yb = eb;
            *m = ef;
        }
        self.see_bg = 0.7 * self.see_bg + 0.3 * seb;
        self.see_fg = 0.7 * self.see_fg + 0.3 * sef;
        if sx <= FAR_ACTIVE_POWER * b as f32 {
            return;
        }
        self.stats.adapted_blocks += 1;
        let erle = 10.0 * ((sd + 1e-12) / (sef + 1e-12)).log10();
        self.stats.erle_db = 0.95 * self.stats.erle_db + 0.05 * erle;
        let mut step = AEC_STEP;
        if self.stats.erle_db > DOUBLE_TALK_ARMED_DB && sd > DOUBLE_TALK_RATIO * (syf + 1e-9) {
            step *= AEC_DOUBLE_TALK_STEP_SCALE;
            self.stats.double_talk_blocks += 1;
        }
        // Background error spectrum from [zeros, error].
        self.time[..b].fill(0.0);
        self.time[b..].copy_from_slice(&self.y_bg);
        self.forward_real(Spectrum::Error);
        // NLMS update of every partition: w += step * conj(X) E / (sum |X|^2 + delta).
        let delta = 1e-6 * (2 * b * b) as f32;
        for p in 0..self.parts {
            let slot = (self.head + p) % self.parts;
            let x = &self.hist[slot * bins..(slot + 1) * bins];
            let w = &mut self.w_bg[p * bins..(p + 1) * bins];
            for (((wk, xk), ek), pk) in w.iter_mut().zip(x).zip(&self.e_spec).zip(&self.power) {
                *wk += xk.conj() * ek * (step / (pk + delta));
            }
        }
        self.stats.complex_macs += (self.parts * bins) as u64;
        // Gradient constraint on one partition per block, round robin: back to time, keep the
        // first half (a causal block-length filter), forward again.
        let p = self.constrain_next;
        self.constrain_next = (p + 1) % self.parts;
        self.acc.copy_from_slice(&self.w_bg[p * bins..(p + 1) * bins]);
        self.inverse_acc();
        self.time[b..].fill(0.0);
        self.forward_real(Spectrum::Weights(p));
        // Two-path transfer and divergence reset.
        if self.see_bg < TRANSFER_RATIO * self.see_fg && self.see_bg < sd + 1e-12 {
            self.good_blocks += 1;
            if self.good_blocks >= TRANSFER_BLOCKS {
                self.w_fg.copy_from_slice(&self.w_bg);
                self.stats.transfers += 1;
            }
        } else {
            self.good_blocks = 0;
        }
        if self.see_bg > RESET_RATIO * self.see_fg && seb > sd {
            self.w_bg.copy_from_slice(&self.w_fg);
            self.see_bg = self.see_fg;
            self.stats.resets += 1;
        }
    }
}

#[derive(Clone, Copy)]
enum Spectrum {
    Error,
    History(usize),
    Weights(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_chat::dsp_fixtures::{
        convolve, energy, room_impulse_response, speech_like, white_noise,
    };

    /// Runs the canceller over `mic` and `far` in 20 ms frames and returns the output.
    fn run(aec: &mut EchoCanceller, mic: &[f32], far: &[f32], rate: u32) -> Vec<f32> {
        let frame = (rate / 50) as usize;
        let mut out = mic.to_vec();
        for (m, f) in out.chunks_mut(frame).zip(far.chunks(frame)) {
            aec.process(m, f);
        }
        out
    }

    fn erle_db(echo: &[f32], residual: &[f32]) -> f64 {
        10.0 * (energy(echo) / energy(residual).max(1e-30)).log10()
    }

    /// ERLE after convergence (ADR-2608 states >= 25 dB from 3 s on; the prototype measured
    /// about 36 dB at 16 kHz and 35 dB at 48 kHz).
    #[test]
    fn erle_after_convergence_meets_the_adr_figure() {
        for rate in [16_000u32, 24_000, 48_000] {
            let n = rate as usize * 6;
            let far = speech_like(n, rate, 1, -20.0);
            let h = room_impulse_response(rate, 1);
            let echo = convolve(&far, &h);
            let noise = white_noise(n, 3, -70.0);
            let mic: Vec<f32> = echo.iter().zip(&noise).map(|(e, v)| e + v).collect();
            let mut aec = EchoCanceller::new(rate);
            let out = run(&mut aec, &mic, &far, rate);
            let residual: Vec<f32> = out.iter().zip(&noise).map(|(o, v)| o - v).collect();
            let from = 3 * rate as usize;
            let erle = erle_db(&echo[from..], &residual[from..]);
            println!("aec {rate} Hz: ERLE {erle:.1} dB over 3-6 s, {:?}", aec.stats());
            assert!(
                erle >= 25.0,
                "{rate} Hz: ERLE {erle:.1} dB after convergence, stats {:?}",
                aec.stats()
            );
            let s = aec.stats();
            assert!(s.transfers > 0 && s.erle_db > 20.0, "{s:?}");
        }
    }

    /// With no far end at all the output is the input, sample for sample.
    #[test]
    fn near_end_only_passes_through_unchanged() {
        let rate = 16_000;
        let n = rate as usize * 2;
        let near = speech_like(n, rate, 9, -20.0);
        let far = vec![0.0f32; n];
        let mut aec = EchoCanceller::new(rate);
        let out = run(&mut aec, &near, &far, rate);
        assert_eq!(out, near);
        assert_eq!(aec.stats().adapted_blocks, 0);
    }

    /// After converging on far-end speech, the far end goes silent and the local player talks:
    /// once the far-end history leaves the filter tail, the output equals the input within
    /// -60 dB of error.
    #[test]
    fn near_end_after_convergence_passes_within_tolerance() {
        let rate = 16_000u32;
        let n = rate as usize * 6;
        let mut far = speech_like(n, rate, 2, -20.0);
        let half = 3 * rate as usize;
        far[half..].fill(0.0);
        let h = room_impulse_response(rate, 2);
        let echo = convolve(&far, &h);
        let mut near = speech_like(n, rate, 11, -20.0);
        near[..half].fill(0.0);
        let mic: Vec<f32> = echo.iter().zip(&near).map(|(e, v)| e + v).collect();
        let mut aec = EchoCanceller::new(rate);
        let out = run(&mut aec, &mic, &far, rate);
        // Skip the tail of the far end (room response plus the filter's 128 ms memory).
        let from = half + rate as usize / 4;
        let diff: Vec<f32> = out[from..]
            .iter()
            .zip(&near[from..])
            .map(|(o, v)| o - v)
            .collect();
        let snr = 10.0 * (energy(&near[from..]) / energy(&diff).max(1e-30)).log10();
        assert!(
            snr >= 60.0,
            "near-end passthrough error {snr:.1} dB below the signal"
        );
    }

    /// Double talk: converge on far-end speech, then the local player talks over it. The output
    /// path keeps cancelling (ERLE >= 20 dB on the echo part) and the local voice survives (output
    /// minus near end is >= 15 dB below the near end).
    #[test]
    fn double_talk_does_not_wreck_the_output_path() {
        let rate = 16_000u32;
        let n = rate as usize * 7;
        let far = speech_like(n, rate, 4, -20.0);
        let h = room_impulse_response(rate, 4);
        let echo = convolve(&far, &h);
        let half = 3 * rate as usize;
        let mut near = speech_like(n, rate, 8, -16.0);
        near[..half].fill(0.0);
        let mic: Vec<f32> = echo.iter().zip(&near).map(|(e, v)| e + v).collect();
        let mut aec = EchoCanceller::new(rate);
        let out = run(&mut aec, &mic, &far, rate);
        let from = half;
        let residual: Vec<f32> = out[from..]
            .iter()
            .zip(&near[from..])
            .map(|(o, v)| o - v)
            .collect();
        let erle = erle_db(&echo[from..], &residual);
        let near_snr = 10.0 * (energy(&near[from..]) / energy(&residual).max(1e-30)).log10();
        let s = aec.stats();
        println!("aec double talk: ERLE {erle:.1} dB, near-end SNR {near_snr:.1} dB, {s:?}");
        assert!(erle >= 20.0, "ERLE during double talk {erle:.1} dB, {s:?}");
        assert!(near_snr >= 15.0, "near-end SNR {near_snr:.1} dB, {s:?}");
        assert!(s.double_talk_blocks > 0, "the detector never fired: {s:?}");
    }

    /// The deterministic cost counters per 20 ms frame: at most 12 FFTs (per 10 ms block: one
    /// far-end, two echo estimates, one error, two for the constraint) and 3 x partitions x bins
    /// complex multiply-accumulates per block.
    #[test]
    fn cost_counters_per_frame() {
        let rate = 48_000u32;
        let frame = 960;
        let far = speech_like(frame * 10, rate, 5, -20.0);
        let mut mic = far.clone();
        let mut aec = EchoCanceller::new(rate);
        for (m, f) in mic.chunks_mut(frame).zip(far.chunks(frame)) {
            aec.process(m, f);
        }
        let s = aec.stats();
        assert_eq!(s.blocks, 20);
        assert!(s.ffts <= 12 * 10, "{s:?}");
        let per_block = (3 * aec.partitions() * (aec.block_samples() + 1)) as u64;
        assert!(s.complex_macs <= per_block * 20, "{s:?}");
        assert_eq!(aec.partitions(), 13);
    }
}
