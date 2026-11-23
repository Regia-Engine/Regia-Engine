// SPDX-License-Identifier: MPL-2.0
//! With/without image-diff metrics for VFX evidence (M14 round 2).
//!
//! An effect render is judged against the *same* frame rendered without the effect: the
//! per-pixel difference is the effect's coverage mask, and every metric (luminance, blow-out,
//! colour histogram) is computed over that mask only. This is robust against the background
//! scene and catches the bug classes the terrain/renderer lanes found (empty/black/blown-out
//! effects, double-applied or missing exposure, NaNs) at a glance:
//!
//! - **empty** — coverage below [`MIN_COVERAGE`]: nothing rendered, or it rendered somewhere
//!   the camera cannot see (bad transforms, culling, winding).
//! - **black** — the effect's pixels are nearly black: colours lost (sRGB applied twice),
//!   alpha multiplied by zero, or a NaN that decoded to 0.
//! - **blown out** — a large fraction of effect pixels pinned at maximum: exposure applied
//!   twice, additive layers stacking to white, or HDR values written into an 8-bit target.
//!
//! Thresholds are deliberately loose bands (not goldens): the GPU simulation's ring-slot
//! assignment is atomic, so particle *rendering order* — never the metrics — varies per run.

/// Per-frame effect metrics (all fractions are 0..=1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectMetrics {
    /// Fraction of frame pixels the effect changed (the diff mask).
    pub coverage: f32,
    /// Mean sRGB luminance of effect pixels (0..=1).
    pub mean_luma: f32,
    /// Fraction of effect pixels at or above 0.985 luminance (blown out).
    pub blown: f32,
    /// Fraction of effect pixels darker than 0.03 luminance (near black).
    pub dark: f32,
    /// Distinct quantised colours (4 bits/channel) among effect pixels.
    pub distinct_colors: usize,
    /// Mean (R + G) / 2 − B over effect pixels: > 0 warm, < 0 cool.
    pub warmth: f32,
    /// Mean absolute per-channel difference inside the mask (0..=1).
    pub mean_delta: f32,
    /// Normalised bounding box of the effect mask `(x0, y0, x1, y1)`, top-left origin;
    /// `None` when empty (the debug view that catches out-of-frame effects).
    pub bbox: Option<(f32, f32, f32, f32)>,
}

/// Minimum visible effect coverage (0.5 % of the frame) — below this the effect is "empty".
pub const MIN_COVERAGE: f32 = 0.005;
/// Minimum coverage for the sparks evidence shot (2 %). Sparks are a handful of tiny
/// stretched streaks, so the evidence scene densifies and frames them tight to measure streak
/// rendering; this floor pins the shot above the generic 0.5 % guard so a regression back
/// toward sub-pixel coverage still fails.
pub const SPARKS_MIN_COVERAGE: f32 = 0.02;
/// Maximum plausible coverage (90 %) — above this the effect floods the frame (projection bug).
pub const MAX_COVERAGE: f32 = 0.90;
/// Effect pixels darker than this fraction of the frame are "black".
pub const MAX_DARK: f32 = 0.70;
/// Blown-out tolerance (additive cores are allowed to saturate; the whole effect is not).
pub const MAX_BLOWN: f32 = 0.35;
/// Minimum distinct quantised colours.
pub const MIN_DISTINCT: usize = 8;

/// Rec. 709 luminance of an sRGB pixel (0..=1).
#[inline]
fn luma(r: u8, g: u8, b: u8) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

/// Measures the effect: `with` and `without` are same-size tightly packed RGBA8 frames
/// (`without` = the same scene, same tick, no effect), `width` pixels wide.
///
/// # Panics
///
/// Panics when the two frames have different lengths, `width` is 0, or `width` does not divide them.
pub fn measure_effect(with: &[u8], without: &[u8], width: u32) -> EffectMetrics {
    assert_eq!(
        with.len(),
        without.len(),
        "with/without frames must be the same size"
    );
    assert!(width > 0, "width must be at least 1");
    assert_eq!(
        with.len() % (width as usize * 4),
        0,
        "width does not divide the frame"
    );
    let px_with = with.as_chunks::<4>().0;
    let px_without = without.as_chunks::<4>().0;
    assert_eq!(px_with.len(), px_without.len());

    let mut effect = 0usize;
    let mut luma_sum = 0.0f64;
    let mut blown = 0usize;
    let mut dark = 0usize;
    let mut warmth_sum = 0.0f64;
    let mut delta_sum = 0.0f64;
    let mut colors = std::collections::HashSet::new();
    let mut bbox: Option<(u32, u32, u32, u32)> = None;
    for (index, (a, b)) in px_with.iter().zip(px_without.iter()).enumerate() {
        let dr = i32::from(a[0]) - i32::from(b[0]);
        let dg = i32::from(a[1]) - i32::from(b[1]);
        let db = i32::from(a[2]) - i32::from(b[2]);
        if dr.abs() + dg.abs() + db.abs() <= 10 {
            continue; // unchanged: not an effect pixel
        }
        effect += 1;
        let (x, y) = (index as u32 % width, index as u32 / width);
        bbox = Some(match bbox {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
        let l = luma(a[0], a[1], a[2]);
        luma_sum += f64::from(l);
        if l >= 0.985 {
            blown += 1;
        }
        if l < 0.03 {
            dark += 1;
        }
        warmth_sum += (f64::from(a[0]) + f64::from(a[1])) * 0.5 / 255.0 - f64::from(a[2]) / 255.0;
        delta_sum += f64::from(dr.abs() + dg.abs() + db.abs()) / 3.0 / 255.0;
        colors.insert((u16::from(a[0] >> 4) << 8) | (u16::from(a[1] >> 4) << 4) | u16::from(a[2] >> 4));
    }
    let n = px_with.len().max(1) as f64;
    let e = effect.max(1) as f64;
    let height = (px_with.len() / width.max(1) as usize) as u32;
    EffectMetrics {
        coverage: effect as f32 / n as f32,
        mean_luma: (luma_sum / e) as f32,
        blown: blown as f32 / e as f32,
        dark: dark as f32 / e as f32,
        distinct_colors: colors.len(),
        warmth: (warmth_sum / e) as f32,
        mean_delta: (delta_sum / e) as f32,
        bbox: bbox.map(|(x0, y0, x1, y1)| {
            (
                x0 as f32 / width as f32,
                y0 as f32 / height.max(1) as f32,
                x1 as f32 / width as f32,
                y1 as f32 / height.max(1) as f32,
            )
        }),
    }
}

/// Asserts the effect is plausibly rendered: not empty, not black, not blown out, not a flat
/// rectangle of one colour.
///
/// # Panics
///
/// Panics with a readable failure when a band is violated (this is the M14 evidence test).
#[allow(clippy::missing_panics_doc)]
pub fn assert_effect_plausible(name: &str, m: &EffectMetrics) {
    assert!(
        m.coverage >= MIN_COVERAGE,
        "{name}: effect is empty (coverage {:.4} < {MIN_COVERAGE})",
        m.coverage
    );
    assert!(
        m.coverage <= MAX_COVERAGE,
        "{name}: effect floods the frame (coverage {:.3} > {MAX_COVERAGE})",
        m.coverage
    );
    assert!(
        m.mean_luma >= 0.05,
        "{name}: effect is black (mean luma {:.4})",
        m.mean_luma
    );
    assert!(
        m.dark <= MAX_DARK,
        "{name}: effect is mostly black (dark fraction {:.3} > {MAX_DARK})",
        m.dark
    );
    assert!(
        m.blown <= MAX_BLOWN,
        "{name}: effect is blown out (blown fraction {:.3} > {MAX_BLOWN})",
        m.blown
    );
    assert!(
        m.distinct_colors >= MIN_DISTINCT,
        "{name}: effect renders as one flat color ({} < {MIN_DISTINCT} distinct)",
        m.distinct_colors
    );
}

/// Measures a whole frame with no diff mask (backdrop sanity checks): every pixel above the
/// near-black skip threshold counts, so `coverage` becomes the fraction of lit pixels and the
/// luma/blown/histogram bands describe the frame itself. Catches the "backdrop rendered black
/// / NaN / double-exposed" class of bugs that would silently invalidate every diff metric.
pub fn measure_frame(pixels: &[u8], width: u32) -> EffectMetrics {
    measure_effect(pixels, &vec![0u8; pixels.len()], width)
}

/// A one-line human-readable summary (printed by `vfx_lab --evidence` and the perf report).
pub fn metrics_line(name: &str, m: &EffectMetrics) -> String {
    let bbox = m
        .bbox
        .map(|(x0, y0, x1, y1)| format!(" bbox [{x0:.2},{y0:.2}]-[{x1:.2},{y1:.2}]"))
        .unwrap_or_else(|| " bbox none".into());
    format!(
        "{name:<14} coverage {:>6.2}%  luma {:>5.3}  blown {:>5.2}%  dark {:>5.2}%  colors {:>3}  warmth {:+.2} {bbox}",
        m.coverage * 100.0,
        m.mean_luma,
        m.blown * 100.0,
        m.dark * 100.0,
        m.distinct_colors,
        m.warmth,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 4;

    fn frame(fill: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        for _ in 0..W * W {
            v.extend_from_slice(&fill);
        }
        v
    }

    #[test]
    fn identical_frames_measure_empty() {
        let f = frame([10, 200, 30, 255]);
        let m = measure_effect(&f, &f, W as u32);
        assert_eq!(m.coverage, 0.0);
    }

    #[test]
    fn measure_frame_counts_lit_fraction() {
        // Half the frame near-black, half lit: coverage is the lit fraction, not 1.0.
        let mut pixels = Vec::new();
        for y in 0..8 {
            for x in 0..16 {
                let c = if y < 4 {
                    [4, 3, 2, 255]
                } else {
                    [90 + x, 120, 150, 255]
                };
                pixels.extend_from_slice(&c);
            }
        }
        let m = measure_frame(&pixels, 16);
        assert!((m.coverage - 0.5).abs() < 1e-3, "coverage {}", m.coverage);
        assert!(m.mean_luma > 0.2, "luma of the lit half: {}", m.mean_luma);
        assert!(m.distinct_colors >= 2);
    }

    #[test]
    fn measure_frame_flags_all_black_backdrop() {
        let m = measure_frame(&frame([0, 0, 0, 255]), W as u32);
        assert_eq!(m.coverage, 0.0);
        assert_eq!(m.distinct_colors, 0);
    }

    #[test]
    fn empty_effect_fails_plausibility() {
        let f = frame([10, 200, 30, 255]);
        let m = measure_effect(&f, &f, W as u32);
        let r = std::panic::catch_unwind(|| assert_effect_plausible("empty", &m));
        assert!(r.is_err(), "empty effect must fail");
    }

    #[test]
    fn blown_out_effect_fails_plausibility() {
        let with = frame([255, 255, 255, 255]);
        let without = frame([10, 200, 30, 255]);
        let m = measure_effect(&with, &without, 16);
        assert_eq!(m.coverage, 1.0);
        let r = std::panic::catch_unwind(|| assert_effect_plausible("blown", &m));
        assert!(r.is_err(), "all-white effect must fail");
    }

    #[test]
    fn black_effect_fails_plausibility() {
        let with = frame([0, 0, 0, 255]);
        let without = frame([10, 200, 30, 255]);
        let m = measure_effect(&with, &without, 16);
        assert_eq!(m.coverage, 1.0);
        let r = std::panic::catch_unwind(|| assert_effect_plausible("black", &m));
        assert!(r.is_err(), "black effect must fail");
    }

    #[test]
    fn gradient_effect_is_plausible() {
        // 16×16 gradient in the top half, background below.
        let mut with = Vec::new();
        for y in 0..16 {
            for x in 0..16 {
                let c = if y < 8 {
                    [(x * 16) as u8, (y * 31) as u8, 120, 255]
                } else {
                    [10, 200, 30, 255]
                };
                with.extend_from_slice(&c);
            }
        }
        let without = frame([10, 200, 30, 255]);
        let without = without.repeat(16); // 16×16 background, same size as `with`
        let m = measure_effect(&with, &without, 16);
        assert_effect_plausible("gradient", &m);
        assert!((m.coverage - 0.5).abs() < 0.01);
    }

    #[test]
    fn warmth_sign_follows_channels() {
        let warm = frame([255, 128, 0, 255]);
        let cool = frame([0, 128, 255, 255]);
        let bg = frame([127, 127, 127, 255]);
        let wm = measure_effect(&warm, &bg, W as u32);
        let cm = measure_effect(&cool, &bg, W as u32);
        assert!(wm.warmth > 0.2, "warm colors measure warm ({})", wm.warmth);
        assert!(cm.warmth < -0.2, "cool colors measure cool ({})", cm.warmth);
    }
    #[test]
    #[should_panic(expected = "width must be at least 1")]
    fn zero_width_reports_a_clear_panic() {
        let _ = measure_effect(&[], &[], 0);
    }
}
