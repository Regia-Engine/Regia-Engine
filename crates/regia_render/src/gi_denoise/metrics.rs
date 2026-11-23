// SPDX-License-Identifier: MPL-2.0
//! Scores for a denoised GI frame or sequence against its ground truth.
//!
//! Every score works on Rec. 709 luminance ([`super::luminance`]) over the pixels a mask
//! selects, accumulates in `f64`, and returns `None` when the mask selects nothing, the
//! buffers disagree in length or the reference is black (a ratio would be meaningless).
//!
//! - [`relative_rmse`]: noise and error, `sqrt(mean((L − L_ref)²)) / mean(L_ref)`.
//! - [`mean_bias`]: energy, `mean(L) / mean(L_ref) − 1` (a filter that darkens is negative).
//! - [`relative_mean_abs_error`]: `mean(|L − L_ref|) / mean(L_ref)`, robust to outliers; used on
//!   shadow-edge bands.
//! - [`temporal_flicker`]: frame-to-frame instability, `mean(|L_t − L_{t−1}|) / mean(L_t)` with
//!   the previous frame read through the motion vectors (nearest pixel). On a static view of a
//!   static scene a perfect result is 0; raw one-sample-per-pixel noise scores about 1.

use super::luminance;

fn ratio(num: f64, den: f64, count: usize) -> Option<f32> {
    if count > 0 && den > 0.0 && num.is_finite() {
        Some((num / den) as f32)
    } else {
        None
    }
}

/// `sqrt(mean((L − L_ref)²)) / mean(L_ref)` over the masked pixels.
#[must_use]
pub fn relative_rmse(test: &[[f32; 3]], reference: &[[f32; 3]], mask: &[bool]) -> Option<f32> {
    if test.len() != reference.len() || test.len() != mask.len() {
        return None;
    }
    let mut sq = 0.0f64;
    let mut sum_ref = 0.0f64;
    let mut count = 0usize;
    for ((t, r), m) in test.iter().zip(reference).zip(mask) {
        if *m {
            let lr = f64::from(luminance(*r));
            let d = f64::from(luminance(*t)) - lr;
            sq += d * d;
            sum_ref += lr;
            count += 1;
        }
    }
    let n = count as f64;
    ratio((sq / n.max(1.0)).sqrt(), sum_ref / n.max(1.0), count)
}

/// `mean(L) / mean(L_ref) − 1` over the masked pixels.
#[must_use]
pub fn mean_bias(test: &[[f32; 3]], reference: &[[f32; 3]], mask: &[bool]) -> Option<f32> {
    if test.len() != reference.len() || test.len() != mask.len() {
        return None;
    }
    let mut sum_t = 0.0f64;
    let mut sum_ref = 0.0f64;
    let mut count = 0usize;
    for ((t, r), m) in test.iter().zip(reference).zip(mask) {
        if *m {
            sum_t += f64::from(luminance(*t));
            sum_ref += f64::from(luminance(*r));
            count += 1;
        }
    }
    ratio(sum_t, sum_ref, count).map(|r| r - 1.0)
}

/// `mean(|L − L_ref|) / mean(L_ref)` over the masked pixels.
#[must_use]
pub fn relative_mean_abs_error(test: &[[f32; 3]], reference: &[[f32; 3]], mask: &[bool]) -> Option<f32> {
    if test.len() != reference.len() || test.len() != mask.len() {
        return None;
    }
    let mut sum_d = 0.0f64;
    let mut sum_ref = 0.0f64;
    let mut count = 0usize;
    for ((t, r), m) in test.iter().zip(reference).zip(mask) {
        if *m {
            let lr = f64::from(luminance(*r));
            sum_d += (f64::from(luminance(*t)) - lr).abs();
            sum_ref += lr;
            count += 1;
        }
    }
    ratio(sum_d, sum_ref, count)
}

/// Mean luminance over the masked pixels.
#[must_use]
pub fn mean_luminance(image: &[[f32; 3]], mask: &[bool]) -> Option<f32> {
    if image.len() != mask.len() {
        return None;
    }
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for (c, m) in image.iter().zip(mask) {
        if *m {
            sum += f64::from(luminance(*c));
            count += 1;
        }
    }
    if count > 0 {
        Some((sum / count as f64) as f32)
    } else {
        None
    }
}

/// One frame pair's flicker, `mean(|L_t − L_{t−1}|) / mean(L_t)`, over the masked pixels of
/// the current frame whose motion lands on screen. `motion` is the current frame's (pixel
/// offsets to the previous position, rounded to the nearest pixel here).
#[must_use]
pub fn temporal_flicker(
    previous: &[[f32; 3]],
    current: &[[f32; 3]],
    motion: &[[f32; 2]],
    mask: &[bool],
    width: u32,
) -> Option<f32> {
    let n = current.len();
    if width == 0
        || previous.len() != n
        || motion.len() != n
        || mask.len() != n
        || !n.is_multiple_of(width as usize)
    {
        return None;
    }
    let w = width as i64;
    let h = (n / width as usize) as i64;
    let mut sum_d = 0.0f64;
    let mut sum_l = 0.0f64;
    let mut count = 0usize;
    for (i, ((c, m), on)) in current.iter().zip(motion).zip(mask).enumerate() {
        if !*on || !m[0].is_finite() || !m[1].is_finite() {
            continue;
        }
        let px = (i as i64 % w) + m[0].round() as i64;
        let py = (i as i64 / w) + m[1].round() as i64;
        if px < 0 || py < 0 || px >= w || py >= h {
            continue;
        }
        let lc = f64::from(luminance(*c));
        let lp = f64::from(luminance(previous[(py * w + px) as usize]));
        sum_d += (lc - lp).abs();
        sum_l += lc;
        count += 1;
    }
    ratio(sum_d, sum_l, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_images_score_zero() {
        let a = vec![[1.0, 2.0, 3.0]; 12];
        let mask = vec![true; 12];
        assert_eq!(relative_rmse(&a, &a, &mask), Some(0.0));
        assert_eq!(mean_bias(&a, &a, &mask), Some(0.0));
        assert_eq!(relative_mean_abs_error(&a, &a, &mask), Some(0.0));
        assert_eq!(temporal_flicker(&a, &a, &[[0.0; 2]; 12], &mask, 4), Some(0.0));
    }

    #[test]
    fn known_values() {
        let reference = vec![[1.0; 3]; 4];
        let test = vec![[1.5; 3], [0.5; 3], [1.5; 3], [0.5; 3]];
        let mask = vec![true; 4];
        let rmse = relative_rmse(&test, &reference, &mask).expect("rmse");
        assert!((rmse - 0.5).abs() < 1e-6, "{rmse}");
        let bias = mean_bias(&test, &reference, &mask).expect("bias");
        assert!(bias.abs() < 1e-6, "{bias}");
        let doubled = vec![[2.0; 3]; 4];
        let bias = mean_bias(&doubled, &reference, &mask).expect("bias");
        assert!((bias - 1.0).abs() < 1e-6, "{bias}");
        let mae = relative_mean_abs_error(&test, &reference, &mask).expect("mae");
        assert!((mae - 0.5).abs() < 1e-6, "{mae}");
    }

    #[test]
    fn masks_and_bad_inputs() {
        let a = vec![[1.0; 3]; 4];
        let b = vec![[9.0; 3]; 4];
        assert_eq!(
            relative_rmse(&b, &a, &[false; 4]),
            None,
            "an empty mask has no score"
        );
        assert_eq!(relative_rmse(&b, &a[..3], &[true; 4]), None, "lengths must agree");
        assert_eq!(
            relative_rmse(&a, &[[0.0; 3]; 4], &[true; 4]),
            None,
            "a black reference"
        );
        let only_first = [true, false, false, false];
        assert_eq!(relative_rmse(&a, &a, &only_first), Some(0.0));
        assert_eq!(
            temporal_flicker(&a, &a, &[[0.0; 2]; 4], &[true; 4], 3),
            None,
            "4 is not rows of 3"
        );
    }

    #[test]
    fn flicker_follows_the_motion() {
        // A 4×1 ramp that moved one pixel left: the previous frame read one pixel to the right.
        let previous = vec![[0.0; 3], [1.0; 3], [2.0; 3], [3.0; 3]];
        let current = vec![[1.0; 3], [2.0; 3], [3.0; 3], [7.0; 3]];
        let motion = vec![[1.0, 0.0]; 4];
        let mask = vec![true; 4];
        // The last pixel's previous position is off screen and is skipped.
        assert_eq!(
            temporal_flicker(&previous, &current, &motion, &mask, 4),
            Some(0.0)
        );
        let still = vec![[0.0; 2]; 4];
        let f = temporal_flicker(&previous, &current, &still, &mask, 4).expect("flicker");
        // |1-0| + |2-1| + |3-2| + |7-3| = 7 over 1 + 2 + 3 + 7 = 13.
        assert!((f - 7.0 / 13.0).abs() < 1e-6, "{f}");
    }
}
