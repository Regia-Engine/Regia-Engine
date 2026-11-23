// SPDX-License-Identifier: MPL-2.0
//! Per-voice propagation DSP (`docs/specs/16-audio.md` § 3c): an arrival delay, two 2-pole
//! low-passes (medium absorption, listener coupling) and an optional seismic coda, wrapped around
//! the voice's kira sound.
//!
//! [`PathData`] wraps any kira [`SoundData`]: it starts the inner sound as usual (so the kira
//! handle — volume, pitch, pan, stop — keeps working) and returns a [`PathSound`] that holds the
//! inner sound back for the delay, filters its output, and keeps rendering the coda tail after the
//! inner sound has finished. Cost per voice: one countdown, two biquads per channel and, for ground
//! paths, four combs and two all-passes on a mono sum — no tracks, no allocation on the audio
//! thread (every buffer is sized in [`PathData::into_sound`] on the game thread).
//!
//! Filters: the RBJ "Audio EQ Cookbook" low-pass biquad (R. Bristow-Johnson, public). Coda: a
//! Schroeder reverberator (M. R. Schroeder, "Natural sounding artificial reverberation", JAES 10,
//! 1962): four parallel feedback combs with mutually prime delays and gains
//! `g = 10^(−3 d / T₆₀)` so the tail falls 60 dB in the material's coda time, then two series
//! all-passes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kira::{
    Frame,
    info::Info,
    sound::{Sound, SoundData},
};

use crate::medium::MAX_CUTOFF_HZ;

/// Highest sample rate the coda buffers are sized for; above it the comb delays shorten.
const MAX_RATE: f32 = 96_000.0;
/// Schroeder's comb delays, seconds.
const COMB_DELAYS: [f32; 4] = [0.0297, 0.0371, 0.0411, 0.0437];
/// Schroeder's all-pass delays, seconds, and gain.
const ALLPASS_DELAYS: [f32; 2] = [0.005, 0.0017];
const ALLPASS_GAIN: f32 = 0.7;

/// Live, per-frame-adjustable parameters of one path voice (game thread writes, audio thread
/// reads once per block). Gains are applied through the kira handle's volume; this carries only
/// what the handle cannot express.
#[derive(Debug)]
pub(crate) struct PathControl {
    absorption_hz: AtomicU32,
    coupling_hz: AtomicU32,
    /// Gain after the coda (the listener's contact fade): leaving the ground silences the
    /// ringing too, which the pre-coda handle volume cannot do.
    post_gain: AtomicU32,
}

impl PathControl {
    pub(crate) fn new(absorption_hz: f32, coupling_hz: Option<f32>) -> Arc<Self> {
        Arc::new(Self {
            absorption_hz: AtomicU32::new(open_if_nan(absorption_hz).to_bits()),
            coupling_hz: AtomicU32::new(open_if_nan(coupling_hz.unwrap_or(MAX_CUTOFF_HZ)).to_bits()),
            post_gain: AtomicU32::new(1.0f32.to_bits()),
        })
    }

    /// Retargets the post-coda gain (smoothed over ~5 ms on the audio thread).
    pub(crate) fn set_post_gain(&self, gain: f32) {
        // A NaN gain would reach every sample of the voice: treat it as silence.
        let gain = if gain.is_nan() { 0.0 } else { gain.clamp(0.0, 1.0) };
        self.post_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    fn post_gain(&self) -> f32 {
        f32::from_bits(self.post_gain.load(Ordering::Relaxed))
    }

    /// Retargets the absorption cutoff.
    pub(crate) fn set_absorption_hz(&self, hz: f32) {
        self.absorption_hz
            .store(checked_cutoff(hz).to_bits(), Ordering::Relaxed);
    }

    /// Retargets the coupling cutoff (`None` = unfiltered).
    pub(crate) fn set_coupling_hz(&self, hz: Option<f32>) {
        self.coupling_hz.store(
            checked_cutoff(hz.unwrap_or(MAX_CUTOFF_HZ)).to_bits(),
            Ordering::Relaxed,
        );
    }

    fn absorption(&self) -> f32 {
        f32::from_bits(self.absorption_hz.load(Ordering::Relaxed))
    }

    fn coupling(&self) -> f32 {
        f32::from_bits(self.coupling_hz.load(Ordering::Relaxed))
    }
}

/// A NaN cutoff (from a NaN position or tuning upstream) opens the filter instead of turning
/// its coefficients, and with them its state, into NaN for the rest of the voice.
fn open_if_nan(hz: f32) -> f32 {
    if hz.is_nan() { MAX_CUTOFF_HZ } else { hz }
}

/// [`open_if_nan`] for values from the game thread, counting the refusal.
fn checked_cutoff(hz: f32) -> f32 {
    if hz.is_nan() {
        crate::sound::note_rejected("filter cutoff");
    }
    open_if_nan(hz)
}

/// How a path voice is rendered.
#[derive(Clone, Debug)]
pub(crate) struct PathSpec {
    /// Hold the sound back this long, seconds.
    pub delay_secs: f64,
    /// Coda (seismic ringing) length, seconds; 0 = none.
    pub coda_secs: f32,
    /// Coda wet level 0..1.
    pub coda_mix: f32,
    /// Shared live cutoffs.
    pub control: Arc<PathControl>,
}

/// A kira sound data wrapped with a propagation path.
pub(crate) struct PathData<D> {
    inner: D,
    spec: PathSpec,
}

impl<D> PathData<D> {
    pub(crate) fn new(inner: D, spec: PathSpec) -> Self {
        Self { inner, spec }
    }
}

impl<D: SoundData> SoundData for PathData<D> {
    type Error = D::Error;
    type Handle = D::Handle;

    fn into_sound(self) -> Result<(Box<dyn Sound>, Self::Handle), Self::Error> {
        let (inner, handle) = self.inner.into_sound()?;
        Ok((Box::new(PathSound::new(inner, self.spec)), handle))
    }
}

/// RBJ low-pass biquad, transposed direct form II, stereo.
#[derive(Clone, Copy, Debug, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: [f32; 2],
    z2: [f32; 2],
    cutoff: f32,
    rate: f32,
    bypass: bool,
}

impl Biquad {
    /// Recomputes the coefficients when the cutoff or rate changed (Q = 1/√2, Butterworth).
    fn set(&mut self, cutoff: f32, rate: f32) {
        let cutoff = open_if_nan(cutoff);
        let rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            48_000.0
        };
        if cutoff == self.cutoff && rate == self.rate {
            return;
        }
        self.cutoff = cutoff;
        self.rate = rate;
        let nyquist_guard = 0.45 * rate;
        // A non-finite cutoff (a NaN distance upstream) must not poison the coefficients.
        // A rate so low that the guard is under the 10 Hz floor cannot be filtered either.
        self.bypass =
            !cutoff.is_finite() || cutoff >= MAX_CUTOFF_HZ.min(nyquist_guard) || nyquist_guard < 10.0;
        if self.bypass {
            return;
        }
        let fc = cutoff.clamp(10.0, nyquist_guard);
        let w0 = std::f32::consts::TAU * fc / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        self.b0 = (1.0 - cos) * 0.5 / a0;
        self.b1 = (1.0 - cos) / a0;
        self.b2 = self.b0;
        self.a1 = -2.0 * cos / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    #[inline]
    fn process(&mut self, channel: usize, x: f32) -> f32 {
        if self.bypass {
            return x;
        }
        if !x.is_finite() {
            // Drop the bad sample and clear the state so later finite samples recover.
            self.z1[channel] = 0.0;
            self.z2[channel] = 0.0;
            return 0.0;
        }
        let y = self.b0 * x + self.z1[channel];
        self.z1[channel] = self.b1 * x - self.a1 * y + self.z2[channel];
        self.z2[channel] = self.b2 * x - self.a2 * y;
        y
    }
}

/// A delay line of fixed capacity with a variable read length.
struct Line {
    buf: Vec<f32>,
    pos: usize,
}

impl Line {
    fn new(secs: f32) -> Self {
        Self {
            buf: vec![0.0; (secs * MAX_RATE).ceil() as usize + 1],
            pos: 0,
        }
    }

    /// The sample written `len` pushes ago (1 = the last one).
    #[inline]
    fn read(&self, len: usize) -> f32 {
        let cap = self.buf.len();
        self.buf[(self.pos + cap - len.clamp(1, cap)) % cap]
    }

    /// Writes the next sample.
    #[inline]
    fn push(&mut self, x: f32) {
        self.buf[self.pos] = x;
        self.pos = (self.pos + 1) % self.buf.len();
    }
}

/// Schroeder coda: 4 combs ∥ → 2 all-passes, mono.
struct Coda {
    combs: Vec<Line>,
    allpasses: Vec<Line>,
    gains: [f32; 4],
    lens: [usize; 4],
    ap_lens: [usize; 2],
    rate: f32,
    mix: f32,
    t60: f32,
}

impl Coda {
    fn new(t60: f32, mix: f32) -> Self {
        Self {
            combs: COMB_DELAYS.iter().map(|&d| Line::new(d)).collect(),
            allpasses: ALLPASS_DELAYS.iter().map(|&d| Line::new(d)).collect(),
            gains: [0.0; 4],
            lens: [1; 4],
            ap_lens: [1; 2],
            rate: 0.0,
            mix: mix.clamp(0.0, 1.0),
            t60: t60.max(0.01),
        }
    }

    fn set_rate(&mut self, rate: f32) {
        if rate == self.rate {
            return;
        }
        self.rate = rate;
        for (i, &d) in COMB_DELAYS.iter().enumerate() {
            self.lens[i] = (d * rate).round() as usize;
            self.gains[i] = 10f32.powf(-3.0 * d / self.t60);
        }
        for (i, &d) in ALLPASS_DELAYS.iter().enumerate() {
            self.ap_lens[i] = (d * rate).round() as usize;
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let mut sum = 0.0;
        for (line, (&len, &gain)) in self.combs.iter_mut().zip(self.lens.iter().zip(&self.gains)) {
            let delayed = line.read(len);
            line.push(x + gain * delayed);
            sum += delayed;
        }
        let mut y = sum * 0.25;
        for (line, &len) in self.allpasses.iter_mut().zip(&self.ap_lens) {
            let delayed = line.read(len);
            let v = y + ALLPASS_GAIN * delayed;
            line.push(v);
            y = delayed - ALLPASS_GAIN * v;
        }
        y * self.mix
    }
}

/// The live path sound (audio thread).
pub(crate) struct PathSound {
    inner: Box<dyn Sound>,
    control: Arc<PathControl>,
    delay_remaining: f64,
    absorption: Biquad,
    coupling: Biquad,
    coda: Option<Coda>,
    /// Smoothed post-coda gain.
    post_gain: f32,
    /// Seconds of tail still to render after the inner sound finished.
    tail_remaining: f64,
    inner_done: bool,
}

impl PathSound {
    fn new(inner: Box<dyn Sound>, spec: PathSpec) -> Self {
        // An infinite delay or coda would keep the sound from ever finishing (it would hold its
        // slot in the mixer for good); NaN already fails the comparisons below.
        let coda = (spec.coda_secs.is_finite() && spec.coda_secs > 0.0 && spec.coda_mix > 0.0)
            .then(|| Coda::new(spec.coda_secs, spec.coda_mix));
        let tail = if coda.is_some() {
            f64::from(spec.coda_secs) + 0.05
        } else {
            0.02
        };
        Self {
            inner,
            post_gain: spec.control.post_gain(),
            control: spec.control,
            delay_remaining: if spec.delay_secs.is_finite() {
                spec.delay_secs.max(0.0)
            } else {
                0.0
            },
            absorption: Biquad::default(),
            coupling: Biquad::default(),
            coda,
            tail_remaining: tail,
            inner_done: false,
        }
    }
}

/// `x`, or 0 when it is NaN or infinite.
#[inline]
fn finite_or_zero(x: f32) -> f32 {
    if x.is_finite() { x } else { 0.0 }
}

/// Exposed for unit tests of the line helper.
#[cfg(test)]
fn line_roundtrip(len: usize, input: &[f32]) -> Vec<f32> {
    let mut line = Line::new(0.001);
    input
        .iter()
        .map(|&x| {
            let out = line.read(len);
            line.push(x);
            out
        })
        .collect()
}

impl Sound for PathSound {
    fn on_start_processing(&mut self) {
        self.inner.on_start_processing();
    }

    fn process(&mut self, out: &mut [Frame], dt: f64, info: &Info) {
        let rate = if dt > 0.0 { (1.0 / dt) as f32 } else { 48_000.0 };
        self.absorption.set(self.control.absorption(), rate);
        self.coupling.set(self.control.coupling(), rate);
        if let Some(coda) = self.coda.as_mut() {
            coda.set_rate(rate);
        }

        // Arrival delay: silence until the wave gets here, then the inner sound.
        let mut start = 0;
        if self.delay_remaining > 0.0 {
            let frames = ((self.delay_remaining / dt.max(1e-9)).ceil() as usize).min(out.len());
            for frame in &mut out[..frames] {
                *frame = Frame::ZERO;
            }
            self.delay_remaining -= frames as f64 * dt;
            start = frames;
        }
        if start < out.len() {
            if self.inner_done || self.inner.finished() {
                self.inner_done = true;
                for frame in &mut out[start..] {
                    *frame = Frame::ZERO;
                }
                self.tail_remaining -= (out.len() - start) as f64 * dt;
            } else {
                self.inner.process(&mut out[start..], dt, info);
            }
        }

        let target = self.control.post_gain();
        // One-pole smoothing, 5 ms time constant.
        let k = 1.0 - (-(dt as f32) / 0.005).exp();
        for frame in out.iter_mut() {
            self.post_gain += (target - self.post_gain) * k;
            // A non-finite input sample would stay in the filter and coda state forever (their
            // feedback never forgets it): it is dropped here, before any state sees it.
            let (left, right) = (finite_or_zero(frame.left), finite_or_zero(frame.right));
            let l = self.coupling.process(0, self.absorption.process(0, left));
            let r = self.coupling.process(1, self.absorption.process(1, right));
            let wet = match self.coda.as_mut() {
                Some(coda) => coda.process(0.5 * (l + r)),
                None => 0.0,
            };
            frame.left = (l + wet) * self.post_gain;
            frame.right = (r + wet) * self.post_gain;
        }
    }

    fn finished(&self) -> bool {
        (self.inner_done || self.inner.finished())
            && self.delay_remaining <= 0.0
            && self.tail_remaining <= 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_with_no_room_for_the_floor_bypasses_instead_of_panicking() {
        let mut biquad = Biquad::default();
        biquad.set(1.0, 20.0);
        assert!(biquad.bypass);
        assert_eq!(biquad.process(0, 0.5), 0.5);
    }

    #[test]
    fn line_delays_by_its_length() {
        let out = line_roundtrip(3, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 1.0, 2.0]);
    }

    fn sine_gain(cutoff: f32, hz: f32) -> f32 {
        let rate = 48_000.0;
        let mut biquad = Biquad::default();
        biquad.set(cutoff, rate);
        let mut peak = 0.0f32;
        for i in 0..48_000 {
            let x = (std::f32::consts::TAU * hz * i as f32 / rate).sin();
            let y = biquad.process(0, x);
            if i > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn biquad_recovers_from_a_non_finite_sample() {
        let mut biquad = Biquad::default();
        biquad.set(500.0, 48_000.0);
        assert!(!biquad.bypass);
        for x in [0.5, f32::NAN, f32::INFINITY, 0.25] {
            let y = biquad.process(0, x);
            assert!(y.is_finite(), "output stays finite for input {x}");
        }
        for _ in 0..64 {
            assert!(biquad.process(0, 0.1).is_finite());
        }
    }

    #[test]
    fn biquad_bypasses_a_non_finite_cutoff() {
        let mut biquad = Biquad::default();
        biquad.set(f32::NAN, 48_000.0);
        assert!(biquad.bypass);
        assert_eq!(biquad.process(1, 0.3), 0.3);
    }

    #[test]
    fn biquad_is_a_butterworth_low_pass() {
        assert!((sine_gain(1_000.0, 100.0) - 1.0).abs() < 0.02, "passband");
        assert!(
            (sine_gain(1_000.0, 1_000.0) - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02,
            "-3 dB at fc"
        );
        assert!(sine_gain(1_000.0, 4_000.0) < 0.07, "-24 dB two octaves up");
        assert_eq!(sine_gain(20_000.0, 5_000.0), 1.0, "bypassed at 20 kHz");
    }

    #[test]
    fn a_nan_cutoff_opens_the_filter_instead_of_poisoning_it() {
        let mut biquad = Biquad::default();
        biquad.set(f32::NAN, 48_000.0);
        assert!(biquad.bypass, "NaN opens the filter");
        biquad.set(1_000.0, f32::NAN);
        let y = biquad.process(0, 0.5);
        assert!(y.is_finite(), "{y}");
        // A real cutoff afterwards filters normally.
        assert!((sine_gain(1_000.0, 100.0) - 1.0).abs() < 0.02);
    }

    #[test]
    fn path_control_scrubs_nan_targets() {
        let control = PathControl::new(f32::NAN, Some(f32::NAN));
        control.set_absorption_hz(f32::NAN);
        control.set_coupling_hz(Some(f32::NAN));
        control.set_post_gain(f32::NAN);
        assert_eq!(control.absorption(), MAX_CUTOFF_HZ);
        assert_eq!(control.coupling(), MAX_CUTOFF_HZ);
        assert_eq!(control.post_gain(), 0.0);
        control.set_post_gain(2.0);
        assert_eq!(control.post_gain(), 1.0);
        assert_eq!(finite_or_zero(f32::INFINITY), 0.0);
        assert_eq!(finite_or_zero(-0.25), -0.25);
    }

    #[test]
    fn coda_rings_for_its_t60() {
        let rate = 48_000.0;
        let mut coda = Coda::new(1.0, 1.0);
        coda.set_rate(rate);
        let energy = |from: usize, to: usize, coda: &mut Coda, first: bool| {
            let mut e = 0.0f32;
            for i in from..to {
                let x = if first && i == 0 { 1.0 } else { 0.0 };
                let y = coda.process(x);
                e += y * y;
            }
            e
        };
        let early = energy(0, 4_800, &mut coda, true);
        let _ = energy(4_800, 48_000, &mut coda, false);
        let late = energy(48_000, 52_800, &mut coda, false);
        // After one T60 the tail is ~60 dB (1e-6 in energy) below its start.
        let drop_db = 10.0 * (late / early).log10();
        assert!((-70.0..-50.0).contains(&drop_db), "drop {drop_db} dB");
    }
}
