// SPDX-License-Identifier: MPL-2.0
//! Synthetic, seeded test signals for the processing stages (ADR-2608): speech-like noise, a
//! room impulse response and white noise. Bit-identical on every platform (PCG32 and f64
//! envelopes); test-only.

use super::rng::Pcg32;

/// Uniform white noise with RMS `level_db` dBFS.
pub fn white_noise(n: usize, seed: u64, level_db: f32) -> Vec<f32> {
    let mut rng = Pcg32::new(seed);
    // A uniform value in [-1, 1) has RMS 1/sqrt(3).
    let scale = 10f32.powf(level_db / 20.0) * 3f32.sqrt();
    (0..n).map(|_| (rng.next_f32() * 2.0 - 1.0) * scale).collect()
}

/// Speech-like noise: white noise through a one-pole low-pass (spectral tilt), amplitude
/// modulated by a 3 Hz syllable envelope with pauses (a half-wave rectified sine), normalized to
/// RMS `level_db` dBFS over the whole signal.
pub fn speech_like(n: usize, rate: u32, seed: u64, level_db: f32) -> Vec<f32> {
    let mut rng = Pcg32::new(seed);
    let mut state = 0.0f32;
    let phase = seed as f64;
    let mut out: Vec<f32> = (0..n)
        .map(|i| {
            let w = rng.next_f32() * 2.0 - 1.0;
            state = 0.9 * state + 0.1 * w;
            let t = i as f64 / f64::from(rate);
            let env = (std::f64::consts::TAU * 3.0 * t + phase).sin().max(0.0).sqrt();
            state * env as f32
        })
        .collect();
    normalize(&mut out, level_db);
    out
}

/// Scales `samples` to RMS `level_db` dBFS (no-op on silence).
pub fn normalize(samples: &mut [f32], level_db: f32) {
    let rms = (energy(samples) / samples.len().max(1) as f64).sqrt();
    if rms > 0.0 {
        let k = 10f64.powf(f64::from(level_db) / 20.0) / rms;
        for s in samples {
            *s = (f64::from(*s) * k) as f32;
        }
    }
}

/// A synthetic room: 8 ms of pure delay, a direct path of 0.5 and an exponentially decaying noise
/// tail with a 60 ms T60, 80 ms long in total (within the canceller's 128 ms tail).
pub fn room_impulse_response(rate: u32, seed: u64) -> Vec<f32> {
    let n = (rate as usize * 80) / 1000;
    let delay = (rate as usize * 8) / 1000;
    let mut rng = Pcg32::new(seed.wrapping_add(100));
    let mut h: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let env = 10f64.powf(-3.0 * t / 0.060);
            ((f64::from(rng.next_f32()) * 2.0 - 1.0) * env * 0.5) as f32
        })
        .collect();
    h[..delay].fill(0.0);
    h[delay] = 0.5;
    h
}

/// Linear convolution truncated to the length of `x` (one f64 FFT; direct form is too slow for
/// seconds of 48 kHz audio in a debug build).
pub fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
    use rustfft::FftPlanner;
    use rustfft::num_complex::Complex;
    let n = (x.len() + h.len()).next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let spectrum = |s: &[f32]| {
        let mut buf = vec![Complex::new(0.0f64, 0.0); n];
        for (b, v) in buf.iter_mut().zip(s) {
            b.re = f64::from(*v);
        }
        fwd.process(&mut buf);
        buf
    };
    let mut a = spectrum(x);
    let b = spectrum(h);
    for (p, q) in a.iter_mut().zip(&b) {
        *p *= q;
    }
    inv.process(&mut a);
    a.iter().take(x.len()).map(|c| (c.re / n as f64) as f32).collect()
}

/// Sum of squares in f64.
pub fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum()
}

/// RMS in dBFS (-200 for silence).
pub fn rms_db(samples: &[f32]) -> f64 {
    let e = energy(samples) / samples.len().max(1) as f64;
    if e <= 0.0 { -200.0 } else { 10.0 * e.log10() }
}
