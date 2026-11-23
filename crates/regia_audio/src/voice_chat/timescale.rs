// SPDX-License-Identifier: MPL-2.0
//! Pitch-synchronous time scaling of decoded voice (`docs/specs/59-voice-chat.md` § 2.9): the jitter
//! buffer drains or fills by a pitch period rather than by a whole frame.
//!
//! The waveform-similarity idea of W. Verhelst and M. Roelands, "An overlap-add technique based on
//! waveform similarity (WSOLA)", ICASSP 1993: find the lag where the signal best matches itself, then
//! overlap-add across that lag so the seam falls on similar waveforms. Original code.

/// Shortest period searched: 2.5 ms (400 Hz).
const MIN_PERIOD_MS: f32 = 2.5;
/// Longest period searched: 10 ms (100 Hz). Two periods must fit in a 20 ms frame.
const MAX_PERIOD_MS: f32 = 10.0;

/// The lag in `min..=max` where `x` best matches itself (normalized cross-correlation).
fn best_period(x: &[f32], min: usize, max: usize) -> usize {
    let mut best = (min, f32::MIN);
    for lag in min..=max {
        let Some(window) = x.len().checked_sub(lag).map(|w| w.min(max)) else {
            break;
        };
        let (a, b) = (&x[..window], &x[lag..lag + window]);
        let (mut ab, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
        for (p, q) in a.iter().zip(b) {
            ab += p * q;
            aa += p * p;
            bb += q * q;
        }
        let score = ab / (aa * bb).sqrt().max(1e-9);
        if score > best.1 {
            best = (lag, score);
        }
    }
    best.0
}

/// The pitch period of the end of `x`, in samples: the lag where the last samples best match the ones
/// before them. Used by concealment that repeats the last period.
#[must_use]
pub fn tail_period(x: &[f32], rate: u32) -> usize {
    let (min, max) = period_range(rate);
    let window = (2 * max).min(x.len());
    let tail: Vec<f32> = x[x.len() - window..].iter().rev().copied().collect();
    best_period(&tail, min, max).min(x.len().max(1))
}

fn period_range(rate: u32) -> (usize, usize) {
    let samples = |ms: f32| (rate as f32 * ms / 1000.0) as usize;
    (samples(MIN_PERIOD_MS).max(1), samples(MAX_PERIOD_MS).max(2))
}

/// Removes about one pitch period from `x` with a crossfade across it. Returns the samples removed
/// (0 when `x` is too short to hold two periods).
pub fn shorten(x: &mut Vec<f32>, rate: u32) -> usize {
    let (min, max) = period_range(rate);
    if x.len() < 2 * max {
        return 0;
    }
    let period = best_period(x, min, max);
    for i in 0..period {
        let t = (i as f32 + 0.5) / period as f32;
        x[i] = x[i] * (1.0 - t) + x[i + period] * t;
    }
    x.drain(period..2 * period);
    period
}

/// Adds about one pitch period to `x`: the first period repeats, crossfaded into the original
/// signal. Returns the samples added (0 when `x` is too short to hold two periods).
pub fn lengthen(x: &mut Vec<f32>, rate: u32) -> usize {
    let (min, max) = period_range(rate);
    if x.len() < 2 * max {
        return 0;
    }
    let period = best_period(x, min, max);
    // 10 ms at 48 kHz. A longer period is outside the profiles this crate plays.
    if period > 480 {
        return 0;
    }
    let mut extra = [0.0f32; 480];
    for i in 0..period {
        let t = (i as f32 + 0.5) / period as f32;
        extra[i] = x[i + period] * (1.0 - t) + x[i] * t;
    }
    let old = x.len();
    x.resize(old + period, 0.0);
    x.copy_within(period..old, period * 2);
    x[period..period * 2].copy_from_slice(&extra[..period]);
    period
}
