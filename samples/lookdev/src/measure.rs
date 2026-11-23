// SPDX-License-Identifier: MIT-0
//! Shot measurements: luminance, contrast, clipping, saturation, tiling, sharpness,
//! and frame-to-frame stability.
//!
//! Luminance is BT.709 (`0.2126 R + 0.7152 G + 0.0722 B`) on the displayed 8-bit
//! values, so the numbers match a PNG opened in an image viewer. Contrast is the
//! standard deviation of that luminance (Peli, "Contrast in complex images", JOSA A
//! 1990, uses the same deviation; the ratio to the mean is not stored because a
//! near-black frame would blow it up). Tiling is the strongest *secondary* normalised
//! autocorrelation peak: the lag-0 lobe of a smooth gradient is ignored, and a peak
//! counts only after the correlation has fallen and risen again (Oppenheim and
//! Schafer, discrete-time signal processing). Sharpness is the RMS of the 4-neighbour
//! Laplacian divided by 255 (variance of the Laplacian, Pech-Pacheco et al., ICPR
//! 2000, in RMS form).

use serde::{Deserialize, Serialize};

/// Per-pass GPU time for one frame, or the mean over a shot, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PassMillis {
    /// Sun-cascade depth pass.
    pub shadow: f32,
    /// Sky gradient.
    pub sky: f32,
    /// Heightfield.
    pub terrain: f32,
    /// Rock instances.
    pub scatter: f32,
    /// Water plane (zero when the scene has no water).
    pub water: f32,
    /// Sum of the passes.
    pub total: f32,
}

impl Default for PassMillis {
    fn default() -> Self {
        Self {
            shadow: 0.0,
            sky: 0.0,
            terrain: 0.0,
            scatter: 0.0,
            water: 0.0,
            total: 0.0,
        }
    }
}

impl PassMillis {
    /// Sums the five passes into [`Self::total`].
    #[must_use]
    pub fn from_passes(shadow: f32, sky: f32, terrain: f32, scatter: f32, water: f32) -> Self {
        Self {
            shadow,
            sky,
            terrain,
            scatter,
            water,
            total: shadow + sky + terrain + scatter + water,
        }
    }

    /// Mean of `samples`. An empty slice is zero.
    #[must_use]
    pub fn mean(samples: &[Self]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let n = samples.len() as f32;
        let acc = samples.iter().fold(Self::default(), |a, s| Self {
            shadow: a.shadow + s.shadow,
            sky: a.sky + s.sky,
            terrain: a.terrain + s.terrain,
            scatter: a.scatter + s.scatter,
            water: a.water + s.water,
            total: a.total + s.total,
        });
        Self {
            shadow: acc.shadow / n,
            sky: acc.sky / n,
            terrain: acc.terrain / n,
            scatter: acc.scatter / n,
            water: acc.water / n,
            total: acc.total / n,
        }
    }
}

/// Spatial measurements of one displayed frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrameStats {
    /// Mean BT.709 luminance, 0..=255.
    pub mean_luminance: f32,
    /// Standard deviation of luminance, 0..=255.
    pub contrast: f32,
    /// Percentage of pixels with luminance below 8.
    pub near_black_pct: f32,
    /// Percentage of pixels with luminance above 250.
    pub blown_pct: f32,
    /// Mean channel spread `(max - min) / max(max, 1)`, 0..=1.
    pub saturation: f32,
    /// Strongest secondary autocorrelation peak (a material repeat), 0..=1.
    /// A smooth gradient scores 0: its correlation only decays away from lag 0.
    pub tiling_peak: f32,
    /// RMS 4-neighbour Laplacian divided by 255.
    pub sharpness: f32,
}

/// Measures `pixels` (tight RGBA8, top row first).
#[must_use]
pub fn frame_stats(pixels: &[u8], width: u32, height: u32) -> FrameStats {
    let count = (width as usize).saturating_mul(height as usize);
    if count == 0 || pixels.len() < count * 4 {
        return FrameStats {
            mean_luminance: 0.0,
            contrast: 0.0,
            near_black_pct: 0.0,
            blown_pct: 0.0,
            saturation: 0.0,
            tiling_peak: 0.0,
            sharpness: 0.0,
        };
    }
    let mut sum = 0.0_f64;
    let mut sum_sq = 0.0_f64;
    let mut sat = 0.0_f64;
    let mut black = 0_u32;
    let mut blown = 0_u32;
    let mut luma = vec![0.0_f32; count];
    for (i, sample) in luma.iter_mut().enumerate() {
        let p = i * 4;
        let r = f32::from(pixels[p]);
        let g = f32::from(pixels[p + 1]);
        let b = f32::from(pixels[p + 2]);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        *sample = y;
        sum += f64::from(y);
        sum_sq += f64::from(y) * f64::from(y);
        let max_c = r.max(g).max(b);
        let min_c = r.min(g).min(b);
        sat += f64::from((max_c - min_c) / max_c.max(1.0));
        if y < 8.0 {
            black += 1;
        }
        if y > 250.0 {
            blown += 1;
        }
    }
    let n = count as f64;
    let mean = sum / n;
    let variance = (sum_sq / n - mean * mean).max(0.0);
    FrameStats {
        mean_luminance: mean as f32,
        contrast: variance.sqrt() as f32,
        near_black_pct: (f64::from(black) / n * 100.0) as f32,
        blown_pct: (f64::from(blown) / n * 100.0) as f32,
        saturation: (sat / n) as f32,
        tiling_peak: tiling_peak(&luma, width as usize, height as usize),
        sharpness: sharpness(&luma, width as usize, height as usize),
    }
}

/// BT.709 luminance plane, one value per pixel, 0..=255.
#[must_use]
pub fn luminance_plane(pixels: &[u8], width: u32, height: u32) -> Vec<f32> {
    let count = (width as usize).saturating_mul(height as usize);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let p = i * 4;
        if p + 2 >= pixels.len() {
            break;
        }
        let r = f32::from(pixels[p]);
        let g = f32::from(pixels[p + 1]);
        let b = f32::from(pixels[p + 2]);
        out.push(0.2126 * r + 0.7152 * g + 0.0722 * b);
    }
    out
}

/// Mean BT.709 luminance of rows `[start_frac, end_frac)` (top of the image is 0).
///
/// The shot list uses the bottom 45% ([`ground_luminance`]) as the ground read.
/// An empty or inverted range is 0.
#[must_use]
pub fn region_mean_luminance(pixels: &[u8], width: u32, height: u32, start_frac: f32, end_frac: f32) -> f32 {
    let count = (width as usize).saturating_mul(height as usize);
    if width == 0 || height == 0 || pixels.len() < count.saturating_mul(4) {
        return 0.0;
    }
    let start = start_frac.clamp(0.0, 1.0);
    let end = end_frac.clamp(start, 1.0);
    let y0 = (start * height as f32).floor() as u32;
    let mut y1 = (end * height as f32).ceil() as u32;
    if y1 <= y0 {
        y1 = (y0 + 1).min(height);
    }
    let y0 = y0.min(height);
    let y1 = y1.min(height);
    if y1 <= y0 {
        return 0.0;
    }
    let mut sum = 0.0_f64;
    let mut n = 0.0_f64;
    for y in y0..y1 {
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            let r = f32::from(pixels[i]);
            let g = f32::from(pixels[i + 1]);
            let b = f32::from(pixels[i + 2]);
            sum += f64::from(0.2126 * r + 0.7152 * g + 0.0722 * b);
            n += 1.0;
        }
    }
    if n == 0.0 { 0.0 } else { (sum / n) as f32 }
}

/// Mean luminance of the bottom 45% of the frame (horizon cameras put the ground there).
#[must_use]
pub fn ground_luminance(pixels: &[u8], width: u32, height: u32) -> f32 {
    region_mean_luminance(pixels, width, height, 0.55, 1.0)
}

/// Mean absolute luminance difference. `1` means identical; `0` means every pixel
/// jumped by a full 255.
#[must_use]
pub fn stability_from_delta(mean_abs_luminance: f32) -> f32 {
    1.0 - (mean_abs_luminance / 255.0).clamp(0.0, 1.0)
}

/// Mean absolute difference of two equal-length luminance planes.
#[must_use]
pub fn mean_abs_delta(previous: &[f32], current: &[f32]) -> f32 {
    let n = previous.len().min(current.len());
    if n == 0 {
        return 0.0;
    }
    let mut acc = 0.0_f64;
    for i in 0..n {
        acc += f64::from((previous[i] - current[i]).abs());
    }
    (acc / n as f64) as f32
}

/// Box-filters `pixels` so the width is `target_width`, preserving aspect.
/// Frames that are already narrower are returned unchanged.
#[must_use]
pub fn downscale_to_width(pixels: &[u8], width: u32, height: u32, target_width: u32) -> (Vec<u8>, u32, u32) {
    if width <= target_width || width < 2 || height < 2 || target_width < 1 {
        return (pixels.to_vec(), width, height);
    }
    let dest_w = target_width;
    let dest_h = ((u64::from(height) * u64::from(dest_w)) / u64::from(width)).max(1) as u32;
    let mut out = vec![0_u8; (dest_w * dest_h) as usize * 4];
    for y in 0..dest_h {
        let y0 = (u64::from(y) * u64::from(height) / u64::from(dest_h)) as u32;
        let y1 = ((u64::from(y + 1) * u64::from(height) / u64::from(dest_h)) as u32)
            .max(y0 + 1)
            .min(height);
        for x in 0..dest_w {
            let x0 = (u64::from(x) * u64::from(width) / u64::from(dest_w)) as u32;
            let x1 = ((u64::from(x + 1) * u64::from(width) / u64::from(dest_w)) as u32)
                .max(x0 + 1)
                .min(width);
            let mut acc = [0_u32; 4];
            let mut n = 0_u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * width + sx) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += u32::from(pixels[i + c]);
                    }
                    n += 1;
                }
            }
            let o = ((y * dest_w + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n.max(1)) as u8;
            }
        }
    }
    (out, dest_w, dest_h)
}

fn sharpness(luma: &[f32], width: usize, height: usize) -> f32 {
    if width < 3 || height < 3 {
        return 0.0;
    }
    let mut acc = 0.0_f64;
    let mut n = 0_u64;
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let c = f64::from(luma[y * width + x]);
            let lap = 4.0 * c
                - f64::from(luma[y * width + x - 1])
                - f64::from(luma[y * width + x + 1])
                - f64::from(luma[(y - 1) * width + x])
                - f64::from(luma[(y + 1) * width + x]);
            acc += lap * lap;
            n += 1;
        }
    }
    if n == 0 {
        0.0
    } else {
        ((acc / n as f64).sqrt() / 255.0) as f32
    }
}

fn tiling_peak(luma: &[f32], width: usize, height: usize) -> f32 {
    let (small, sw, sh) = if width > 96 {
        let tw = 96_usize;
        let th = (height * tw / width).max(2);
        (box_luma(luma, width, height, tw, th), tw, th)
    } else {
        (luma.to_vec(), width, height)
    };
    if sw < 8 || sh < 4 {
        return 0.0;
    }
    let mean = small.iter().sum::<f32>() / small.len() as f32;
    let var = small
        .iter()
        .map(|v| {
            let d = f64::from(v - mean);
            d * d
        })
        .sum::<f64>()
        / small.len() as f64;
    if var < 1.0 {
        return 0.0;
    }
    let horizontal = autocorr_series(&small, sw, sh, true, mean, var);
    let vertical = autocorr_series(&small, sw, sh, false, mean, var);
    secondary_peak(&horizontal)
        .max(secondary_peak(&vertical))
        .clamp(0.0, 1.0)
}

/// Autocorrelation for lags `1..=max_lag`, one entry per lag.
fn autocorr_series(
    luma: &[f32],
    width: usize,
    height: usize,
    horizontal: bool,
    mean: f32,
    var: f64,
) -> Vec<f32> {
    let extent = if horizontal { width } else { height };
    let max_lag = (extent / 3).max(2).min(extent.saturating_sub(1));
    let mut series = Vec::with_capacity(max_lag);
    for lag in 1..=max_lag {
        series.push(normalised_corr(luma, width, height, lag, horizontal, mean, var));
    }
    series
}

/// Strongest local maximum that follows a real descent from the lag-0 lobe.
///
/// A smooth gradient (and a flat row of a one-axis gradient) never rises again, so
/// this returns 0. A repeated tile falls, then peaks near 1 at the period.
fn secondary_peak(corr: &[f32]) -> f32 {
    let mut peak = 0.0_f32;
    let mut seen_descent = false;
    for k in 1..corr.len() {
        if corr[k] < corr[k - 1] - 0.01 {
            seen_descent = true;
        }
        let rises = corr[k] > corr[k - 1] + 0.02;
        let not_still_rising = k + 1 >= corr.len() || corr[k] >= corr[k + 1];
        if seen_descent && rises && not_still_rising && corr[k] > 0.15 {
            peak = peak.max(corr[k]);
        }
    }
    peak
}

fn box_luma(luma: &[f32], width: usize, height: usize, tw: usize, th: usize) -> Vec<f32> {
    let mut out = vec![0.0_f32; tw * th];
    for y in 0..th {
        let y0 = y * height / th;
        let y1 = ((y + 1) * height / th).max(y0 + 1).min(height);
        for x in 0..tw {
            let x0 = x * width / tw;
            let x1 = ((x + 1) * width / tw).max(x0 + 1).min(width);
            let mut acc = 0.0_f64;
            let mut n = 0_u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    acc += f64::from(luma[sy * width + sx]);
                    n += 1;
                }
            }
            out[y * tw + x] = (acc / f64::from(n.max(1))) as f32;
        }
    }
    out
}

fn normalised_corr(
    luma: &[f32],
    width: usize,
    height: usize,
    lag: usize,
    horizontal: bool,
    mean: f32,
    var: f64,
) -> f32 {
    if lag == 0 || (horizontal && lag >= width) || (!horizontal && lag >= height) {
        return 0.0;
    }
    let mut acc = 0.0_f64;
    let mut n = 0_u64;
    if horizontal {
        for y in 0..height {
            for x in 0..width - lag {
                let a = f64::from(luma[y * width + x] - mean);
                let b = f64::from(luma[y * width + x + lag] - mean);
                acc += a * b;
                n += 1;
            }
        }
    } else {
        for y in 0..height - lag {
            for x in 0..width {
                let a = f64::from(luma[y * width + x] - mean);
                let b = f64::from(luma[(y + lag) * width + x] - mean);
                acc += a * b;
                n += 1;
            }
        }
    }
    if n == 0 {
        0.0
    } else {
        (acc / (n as f64 * var)).clamp(0.0, 1.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((w * h) as usize * 4);
        for _ in 0..w * h {
            pixels.extend(rgba);
        }
        pixels
    }

    #[test]
    fn flat_grey_is_low_contrast_and_stable() {
        let pixels = fill(32, 16, [40, 40, 40, 255]);
        let stats = frame_stats(&pixels, 32, 16);
        assert!(
            (stats.mean_luminance - 40.0).abs() < 0.2,
            "{}",
            stats.mean_luminance
        );
        assert!(stats.contrast < 0.5);
        assert!(stats.saturation < 0.01);
        assert!(stats.sharpness < 0.01);
        assert!(stats.tiling_peak < 0.05);
        assert_eq!(stats.near_black_pct, 0.0);
        assert_eq!(stats.blown_pct, 0.0);
        let plane = luminance_plane(&pixels, 32, 16);
        assert_eq!(mean_abs_delta(&plane, &plane), 0.0);
        assert_eq!(stability_from_delta(0.0), 1.0);
    }

    #[test]
    fn black_and_white_clipping_percentages() {
        let black = frame_stats(&fill(8, 8, [0, 0, 0, 255]), 8, 8);
        assert!((black.near_black_pct - 100.0).abs() < 0.01);
        let white = frame_stats(&fill(8, 8, [255, 255, 255, 255]), 8, 8);
        assert!((white.blown_pct - 100.0).abs() < 0.01);
        assert!(white.mean_luminance > 250.0);
    }

    #[test]
    fn saturated_pixel_and_checker_sharpness() {
        let red = frame_stats(&fill(8, 8, [255, 0, 0, 255]), 8, 8);
        assert!(red.saturation > 0.9, "{}", red.saturation);
        let mut checker = vec![0_u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let on = (x / 4 + y / 4) % 2 == 0;
                let v = if on { 255 } else { 0 };
                let i = (y * 32 + x) * 4;
                checker[i] = v;
                checker[i + 1] = v;
                checker[i + 2] = v;
                checker[i + 3] = 255;
            }
        }
        let stats = frame_stats(&checker, 32, 32);
        let flat = frame_stats(&fill(32, 32, [128, 128, 128, 255]), 32, 32);
        assert!(
            stats.sharpness > flat.sharpness + 0.05,
            "{} vs {}",
            stats.sharpness,
            flat.sharpness
        );
        assert!(stats.tiling_peak > 0.2, "tiling {}", stats.tiling_peak);
    }

    #[test]
    fn a_smooth_gradient_is_not_a_tiling_peak() {
        let mut across = vec![0_u8; 64 * 32 * 4];
        for y in 0..32 {
            for x in 0..64 {
                let v = (x * 255 / 63) as u8;
                let i = (y * 64 + x) * 4;
                across[i] = v;
                across[i + 1] = v;
                across[i + 2] = v;
                across[i + 3] = 255;
            }
        }
        let stats = frame_stats(&across, 64, 32);
        assert!(
            stats.tiling_peak < 0.15,
            "horizontal gradient tiling {}",
            stats.tiling_peak
        );

        let mut down = vec![0_u8; 48 * 32 * 4];
        for y in 0..32 {
            let v = (y * 255 / 31) as u8;
            for x in 0..48 {
                let i = (y * 48 + x) * 4;
                down[i] = v;
                down[i + 1] = v;
                down[i + 2] = v;
                down[i + 3] = 255;
            }
        }
        let stats = frame_stats(&down, 48, 32);
        assert!(
            stats.tiling_peak < 0.15,
            "vertical gradient tiling {}",
            stats.tiling_peak
        );
    }

    #[test]
    fn ground_luminance_reads_the_bottom_of_the_frame() {
        let mut pixels = vec![0_u8; 8 * 10 * 4];
        for y in 0..10 {
            let v = if y < 5 { 200 } else { 10 };
            for x in 0..8 {
                let i = (y * 8 + x) * 4;
                pixels[i] = v;
                pixels[i + 1] = v;
                pixels[i + 2] = v;
                pixels[i + 3] = 255;
            }
        }
        let ground = ground_luminance(&pixels, 8, 10);
        assert!(ground < 20.0, "black ground read as {ground}");
        let sky = region_mean_luminance(&pixels, 8, 10, 0.0, 0.5);
        assert!(sky > 180.0, "upper half {sky}");
    }

    #[test]
    fn downscale_halves_a_two_by_two_block() {
        let mut pixels = vec![0_u8; 4 * 2 * 4];
        for y in 0..2 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                pixels[i] = if x < 2 { 0 } else { 100 };
                pixels[i + 1] = pixels[i];
                pixels[i + 2] = pixels[i];
                pixels[i + 3] = 255;
            }
        }
        let (out, w, h) = downscale_to_width(&pixels, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(out[0], 0);
        assert_eq!(out[4], 100);
    }
}
