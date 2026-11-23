// SPDX-License-Identifier: MPL-2.0
//! Temporal stability metrics over a sequence of frames: steps, pops, flicker, drift and
//! ghosting.
//!
//! A frame is one value per sample (a pixel's luma, a probe's irradiance, the light at a world
//! point), in any unit. Every frame of a sequence holds the same samples in the same order.
//! [`measure`] compares consecutive frames, and each frame against an optional reference (the
//! converged image of a held camera):
//!
//! - **Step:** |Δ| between consecutive frames, per sample: its mean, 99th percentile and
//!   maximum, and the frame pair with the largest mean. A pop (a band that jumps when a probe
//!   volume scrolls) shows in the percentile and the maximum while the mean stays small.
//! - **Temporal information:** ITU-T P.910's TI, the largest (over frame pairs) standard
//!   deviation over the samples of the frame difference.
//! - **Flicker:** the oscillating part of the change. Where a sample's change reverses sign
//!   from one frame pair to the next, the smaller of the two |Δ| counts. A steady drift (a
//!   probe converging, a fade) counts nothing.
//! - **Drift:** mean |last − first| per sample.
//! - **Ghosting** (with a reference): the mean |frame − reference| of each frame, its mean and
//!   largest value over the sequence, the last frame's, and the frame from which it stays within
//!   a tolerance.
//!
//! In a scene the camera's motion leaves unchanged (a long corridor walked along its axis), a
//! correct renderer draws the same image every frame. Every one of these numbers is then an
//! error of the renderer's temporal path, not parallax (`tests/ddgi_temporal.rs`).
//!
//! ## Provenance
//! Original code. Temporal information: ITU-T Recommendation P.910, "Subjective video quality
//! assessment methods for multimedia applications", § 5.3 (TI = max over time of the spatial
//! standard deviation of the frame difference). The flicker, drift and ghosting terms are this
//! module's own definitions.

/// Bins of the |Δ| histogram the 99th percentile reads (resolution `step_max / 1024`).
const HISTOGRAM_BINS: usize = 1024;

/// What [`measure`] reports about a sequence. Every value is in the unit of the samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TemporalStability {
    /// Frames measured.
    pub frames: usize,
    /// Samples per frame.
    pub samples: usize,
    /// Mean |Δ| between consecutive frames, over every sample and frame pair.
    pub step_mean: f32,
    /// 99th percentile of |Δ| over every sample and frame pair: the largest |Δ| in the
    /// histogram bin (1/1024 of [`Self::step_max`] wide) that holds the percentile.
    pub step_p99: f32,
    /// Largest |Δ| of any sample between consecutive frames.
    pub step_max: f32,
    /// Index of the later frame of the pair with the largest mean |Δ|.
    pub worst_step_frame: usize,
    /// ITU-T P.910 temporal information: the largest, over frame pairs, standard deviation of
    /// the frame difference over the samples.
    pub temporal_information: f32,
    /// Mean, per sample and interior frame, of the oscillating part of the change: where the
    /// change reverses sign from one frame pair to the next, the smaller |Δ| of the two. 0 for
    /// two frames.
    pub flicker: f32,
    /// Mean |last − first| per sample.
    pub drift: f32,
    /// Error against the reference, when [`measure`] got one.
    pub ghost: Option<GhostStats>,
}

/// Error of each frame against a reference (the image the sequence should hold).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GhostStats {
    /// Mean over the frames of each frame's mean |frame − reference|.
    pub mean: f32,
    /// The largest per-frame mean |frame − reference|.
    pub max: f32,
    /// The last frame's mean |frame − reference|.
    pub last: f32,
    /// Index of the first frame from which every frame's mean error stays within the
    /// tolerance [`measure`] got. `None` when the last frame's does not.
    pub settle_frame: Option<usize>,
}

/// Why [`measure`] refused a sequence.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum TemporalError {
    /// A sequence needs at least two frames.
    #[error("a sequence needs at least two frames (got {0})")]
    TooFewFrames(usize),
    /// The first frame has no samples.
    #[error("the frames have no samples")]
    Empty,
    /// A frame's sample count differs from the first frame's.
    #[error("frame {frame} has {found} samples, the first frame {expected}")]
    SampleCount {
        /// Index of the frame.
        frame: usize,
        /// Samples in the first frame.
        expected: usize,
        /// Samples in this frame.
        found: usize,
    },
    /// The reference's sample count differs from the frames'.
    #[error("the reference has {found} samples, the frames {expected}")]
    ReferenceSize {
        /// Samples per frame.
        expected: usize,
        /// Samples in the reference.
        found: usize,
    },
    /// A frame (or the reference, reported as frame `usize::MAX`) holds a NaN or an infinity.
    #[error("frame {0} holds a value that is not finite")]
    NotFinite(usize),
}

/// Measures a sequence of `frames` (at least two, the same samples each) and, with a
/// `reference`, the error of each frame against it. `settle_tolerance` is the per-frame mean
/// error under which [`GhostStats::settle_frame`] counts a frame as settled.
///
/// # Errors
/// [`TemporalError`] for fewer than two frames, empty frames, frames or a reference of
/// different sizes, or values that are not finite.
pub fn measure<F: AsRef<[f32]>>(
    frames: &[F],
    reference: Option<&[f32]>,
    settle_tolerance: f32,
) -> Result<TemporalStability, TemporalError> {
    if frames.len() < 2 {
        return Err(TemporalError::TooFewFrames(frames.len()));
    }
    let samples = frames[0].as_ref().len();
    if samples == 0 {
        return Err(TemporalError::Empty);
    }
    for (index, frame) in frames.iter().enumerate() {
        let frame = frame.as_ref();
        if frame.len() != samples {
            return Err(TemporalError::SampleCount {
                frame: index,
                expected: samples,
                found: frame.len(),
            });
        }
        if !frame.iter().all(|v| v.is_finite()) {
            return Err(TemporalError::NotFinite(index));
        }
    }
    if let Some(reference) = reference {
        if reference.len() != samples {
            return Err(TemporalError::ReferenceSize {
                expected: samples,
                found: reference.len(),
            });
        }
        if !reference.iter().all(|v| v.is_finite()) {
            return Err(TemporalError::NotFinite(usize::MAX));
        }
    }

    let pairs = frames.len() - 1;
    let mut step_sum = 0.0f64;
    let mut step_max = 0.0f32;
    let mut worst = (0.0f64, 1usize);
    let mut ti = 0.0f64;
    let mut flicker_sum = 0.0f64;
    let mut prev_delta = vec![0.0f32; samples];
    for (k, pair) in frames.windows(2).enumerate() {
        let t = k + 1;
        let (a, b) = (pair[0].as_ref(), pair[1].as_ref());
        let (mut pair_abs, mut pair_sum, mut pair_sq) = (0.0f64, 0.0f64, 0.0f64);
        for ((&x, &y), prev) in a.iter().zip(b).zip(prev_delta.iter_mut()) {
            let d = y - x;
            let ad = d.abs();
            pair_abs += f64::from(ad);
            pair_sum += f64::from(d);
            pair_sq += f64::from(d) * f64::from(d);
            step_max = step_max.max(ad);
            if t >= 2 && d * *prev < 0.0 {
                flicker_sum += f64::from(ad.min(prev.abs()));
            }
            *prev = d;
        }
        let n = samples as f64;
        let mean = pair_sum / n;
        ti = ti.max((pair_sq / n - mean * mean).max(0.0).sqrt());
        if pair_abs > worst.0 {
            worst = (pair_abs, t);
        }
        step_sum += pair_abs;
    }

    // 99th percentile from a histogram over [0, step_max] (no copy of every |Δ|): the largest
    // value in the bin that holds it.
    let step_p99 = if step_max > 0.0 {
        let mut bins = vec![(0u64, 0.0f32); HISTOGRAM_BINS];
        let scale = HISTOGRAM_BINS as f32 / step_max;
        for pair in frames.windows(2) {
            for (&x, &y) in pair[0].as_ref().iter().zip(pair[1].as_ref()) {
                let ad = (y - x).abs();
                let bin = &mut bins[((ad * scale) as usize).min(HISTOGRAM_BINS - 1)];
                bin.0 += 1;
                bin.1 = bin.1.max(ad);
            }
        }
        let total = (pairs * samples) as u64;
        // The first bin with at least 99 % of the values at or below it.
        let want = (total * 99).div_ceil(100);
        let mut seen = 0u64;
        let mut p99 = step_max;
        for &(count, largest) in &bins {
            seen += count;
            if seen >= want {
                p99 = largest;
                break;
            }
        }
        p99
    } else {
        0.0
    };

    let (first, last) = (frames[0].as_ref(), frames[pairs].as_ref());
    let drift = first
        .iter()
        .zip(last)
        .map(|(&a, &b)| f64::from((b - a).abs()))
        .sum::<f64>()
        / samples as f64;
    let interior = (frames.len() - 2) * samples;
    let flicker = if interior == 0 {
        0.0
    } else {
        flicker_sum / interior as f64
    };

    let ghost = reference.map(|reference| ghost_stats(frames, reference, settle_tolerance));
    Ok(TemporalStability {
        frames: frames.len(),
        samples,
        step_mean: (step_sum / (pairs * samples) as f64) as f32,
        step_p99,
        step_max,
        worst_step_frame: worst.1,
        temporal_information: ti as f32,
        flicker: flicker as f32,
        drift: drift as f32,
        ghost,
    })
}

/// Per-frame mean error against `reference` (sizes checked by [`measure`]).
fn ghost_stats<F: AsRef<[f32]>>(frames: &[F], reference: &[f32], settle_tolerance: f32) -> GhostStats {
    let errors: Vec<f32> = frames
        .iter()
        .map(|frame| {
            let sum: f64 = frame
                .as_ref()
                .iter()
                .zip(reference)
                .map(|(&v, &r)| f64::from((v - r).abs()))
                .sum();
            (sum / reference.len() as f64) as f32
        })
        .collect();
    let mean = errors.iter().map(|&e| f64::from(e)).sum::<f64>() / errors.len() as f64;
    let max = errors.iter().copied().fold(0.0f32, f32::max);
    let last = errors.last().copied().unwrap_or(0.0);
    // The trailing run of frames within the tolerance.
    let settle_frame = if last <= settle_tolerance {
        let outside = errors.iter().rposition(|&e| e > settle_tolerance);
        Some(outside.map_or(0, |i| i + 1))
    } else {
        None
    };
    GhostStats {
        mean: mean as f32,
        max,
        last,
        settle_frame,
    }
}

/// Display-encoded BT.709 luma (0–255, as the 8-bit channels hold it) of the pixels of an RGBA8
/// image inside `region` (`[x0, y0, x1, y1]`, end exclusive, clamped to the image; `None` is
/// the whole image), row by row. Rows the buffer does not hold are left out.
#[must_use]
pub fn rgba8_luma(rgba: &[u8], width: u32, height: u32, region: Option<[u32; 4]>) -> Vec<f32> {
    let held_rows = (rgba.len() / 4).checked_div(width as usize).unwrap_or(0);
    let height = height.min(u32::try_from(held_rows).unwrap_or(u32::MAX));
    let [x0, y0, x1, y1] = region.unwrap_or([0, 0, width, height]);
    let (x1, y1) = (x1.min(width), y1.min(height));
    let mut out = Vec::with_capacity((x1.saturating_sub(x0) as usize) * (y1.saturating_sub(y0) as usize));
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y as usize) * (width as usize) + x as usize) * 4;
            if let Some(p) = rgba.get(i..i + 3) {
                out.push(0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_sequence_measures_zero() {
        let frames = vec![vec![3.0f32; 16]; 6];
        let m = measure(&frames, Some(&[3.0; 16][..]), 0.01).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((m.frames, m.samples), (6, 16));
        assert_eq!(m.step_mean, 0.0);
        assert_eq!(m.step_p99, 0.0);
        assert_eq!(m.step_max, 0.0);
        assert_eq!(m.flicker, 0.0);
        assert_eq!(m.drift, 0.0);
        assert_eq!(m.temporal_information, 0.0);
        let ghost = m.ghost.unwrap_or_else(|| panic!("a reference was given"));
        assert_eq!(ghost.settle_frame, Some(0));
        assert_eq!(ghost.max, 0.0);
    }

    #[test]
    fn alternating_frames_are_flicker_and_a_ramp_is_drift() {
        // ±1 around 10 every frame: every interior frame reverses, each |Δ| = 2.
        let flicker: Vec<Vec<f32>> = (0..9u32)
            .map(|t| vec![if t.is_multiple_of(2) { 9.0 } else { 11.0 }; 4])
            .collect();
        let m = measure(&flicker, None, 0.0).unwrap_or_else(|e| panic!("{e}"));
        assert!((m.step_mean - 2.0).abs() < 1e-6, "{m:?}");
        assert!((m.flicker - 2.0).abs() < 1e-6, "{m:?}");
        assert_eq!(m.drift, 0.0);
        // The same |Δ| as a steady ramp: no flicker at all, the whole change is drift.
        let ramp: Vec<Vec<f32>> = (0..9).map(|t| vec![t as f32 * 2.0; 4]).collect();
        let r = measure(&ramp, None, 0.0).unwrap_or_else(|e| panic!("{e}"));
        assert!((r.step_mean - 2.0).abs() < 1e-6, "{r:?}");
        assert_eq!(r.flicker, 0.0);
        assert!((r.drift - 16.0).abs() < 1e-5, "{r:?}");
        // A uniform change has no spread over the samples: P.910 TI is 0 for both.
        assert!(m.temporal_information < 1e-6 && r.temporal_information < 1e-6);
    }

    #[test]
    fn a_pop_in_a_few_samples_shows_in_the_tail_not_the_mean() {
        // 1000 samples held; between frames 4 and 5, 20 of them (2 %) jump by 8.
        let frames: Vec<Vec<f32>> = (0..10)
            .map(|t| {
                (0..1000)
                    .map(|i| if i < 20 && t >= 5 { 8.0 } else { 0.0 })
                    .collect()
            })
            .collect();
        let m = measure(&frames, None, 0.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(m.step_max, 8.0);
        assert_eq!(m.worst_step_frame, 5);
        // 20 of 9000 values moved: the mean is tiny, the 99th percentile still 0, the max 8.
        assert!(m.step_mean < 0.02, "{m:?}");
        assert_eq!(m.step_p99, 0.0);
        assert_eq!(m.flicker, 0.0, "one step, no reversal");
        // TI sees the spread of that one difference: sqrt(p (1 − p)) · 8 with p = 0.02.
        assert!(
            (m.temporal_information - 8.0 * (0.02f32 * 0.98).sqrt()).abs() < 1e-4,
            "{m:?}"
        );
        // With more than 1 % of the values moving the percentile reads the jump.
        let wide: Vec<Vec<f32>> = (0..2)
            .map(|t| {
                (0..100)
                    .map(|i| if i < 5 && t == 1 { 8.0 } else { 0.0 })
                    .collect()
            })
            .collect();
        let w = measure(&wide, None, 0.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(w.step_p99, 8.0);
    }

    #[test]
    fn ghosting_settles_when_the_error_stays_inside_the_tolerance() {
        // Converging toward 0 by halves from 16, with one late blip back to 2 at frame 6.
        let errors = [16.0f32, 8.0, 4.0, 2.0, 1.0, 0.5, 2.0, 0.25, 0.125];
        let frames: Vec<Vec<f32>> = errors.iter().map(|&e| vec![e; 3]).collect();
        let m = measure(&frames, Some(&[0.0; 3][..]), 1.0).unwrap_or_else(|e| panic!("{e}"));
        let ghost = m.ghost.unwrap_or_else(|| panic!("a reference was given"));
        assert_eq!(ghost.max, 16.0);
        assert_eq!(ghost.last, 0.125);
        assert_eq!(
            ghost.settle_frame,
            Some(7),
            "the blip at frame 6 restarts the count"
        );
        // A last frame outside the tolerance has not settled.
        let late = measure(&frames[..7], Some(&[0.0; 3][..]), 1.0).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(late.ghost.and_then(|g| g.settle_frame), None);
    }

    #[test]
    fn malformed_sequences_are_refused() {
        assert_eq!(
            measure(&[vec![1.0f32]], None, 0.0),
            Err(TemporalError::TooFewFrames(1))
        );
        let empty: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
        assert_eq!(measure(&empty, None, 0.0), Err(TemporalError::Empty));
        assert_eq!(
            measure(&[vec![1.0f32, 2.0], vec![1.0]], None, 0.0),
            Err(TemporalError::SampleCount {
                frame: 1,
                expected: 2,
                found: 1
            })
        );
        assert_eq!(
            measure(&[vec![1.0f32], vec![f32::NAN]], None, 0.0),
            Err(TemporalError::NotFinite(1))
        );
        assert_eq!(
            measure(&[vec![1.0f32], vec![1.0]], Some(&[1.0, 2.0][..]), 0.0),
            Err(TemporalError::ReferenceSize {
                expected: 1,
                found: 2
            })
        );
    }

    #[test]
    fn luma_reads_the_region_row_by_row() {
        // 3 × 2 image: pure red, green, blue on the first row, white, black, grey on the second.
        let rgba: Vec<u8> = [
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [255, 255, 255],
            [0, 0, 0],
            [100, 100, 100],
        ]
        .iter()
        .flat_map(|c| [c[0], c[1], c[2], 255])
        .collect();
        let all = rgba8_luma(&rgba, 3, 2, None);
        assert_eq!(all.len(), 6);
        assert!((all[0] - 0.2126 * 255.0).abs() < 1e-3);
        assert!((all[3] - 255.0).abs() < 1e-3);
        assert!((all[5] - 100.0).abs() < 1e-3);
        // The right column, with an end past the image clamped.
        let column = rgba8_luma(&rgba, 3, 2, Some([2, 0, 9, 9]));
        assert_eq!(column.len(), 2);
        assert!((column[0] - 0.0722 * 255.0).abs() < 1e-3);
        // A short buffer (one row held) yields only that row.
        assert_eq!(rgba8_luma(&rgba[..12], 3, 2, None).len(), 3);
        assert!(rgba8_luma(&rgba, 0, 2, None).is_empty());
    }
}
