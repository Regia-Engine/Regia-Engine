// SPDX-License-Identifier: MIT-0
//! Look CI: the look-regression check run at every green tag (`look_ci` bin,
//! `book/user-guide/look-ci.md`).
//!
//! A tag's render set is a directory with one sub-directory per tier ([`TIERS`]) holding the
//! twelve parity plates ([`R2Shot::PARITY`], `look_r2 --set parity` framing) as `<shot>.png`, and
//! the `do27_chase --plates` per-frame CSV as [`TIMING_CSV`]. Comparing a tag against the previous
//! one flags three things:
//!
//! - **Frame change:** FLIP (Andersson et al., HPG 2020, `regia_render::compare`) of the plate
//!   against the previous tag's above [`FLIP_NOISE_FACTOR`] × the noise floor measured between
//!   two renders of the same tree ([`NoiseFloor`]), never below [`FLIP_FLOOR`].
//! - **UE drift:** a plate's gap to the UE plate grows by more than [`UE_DRIFT_LIMIT`] (luma,
//!   contrast and saturation as a fraction of the UE value) or [`HUE_DRIFT_LIMIT_DEG`] (hue).
//! - **GPU cost:** a plate's GPU frame p50 rises by more than [`GPU_LIMIT`].
//!
//! Plate statistics are the look-r2 measures: BT.709 luma of the displayed 8-bit values, its
//! standard deviation as contrast (Peli, JOSA A 1990, as in [`crate::measure`]), mean HSV
//! saturation, and the circular mean hue of the saturated pixels (HSV S ≥ 0.35, V ≥ 0.12, the mask
//! the look-r2 hue tests use). Statistics are always recomputed from the PNGs by one build of this
//! module, so tags rendered by older harnesses compare on the same definitions.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::look_r2::R2Shot;

/// Tier sub-directories of a render set and the `LOOK_R2_TIER` value each renders with.
pub const TIERS: [&str; 2] = ["desktop-high", "deck"];

/// File name of the `do27_chase --plates --csv` timing run inside a render set.
pub const TIMING_CSV: &str = "timing.csv";

/// The FLIP threshold is this multiple of the noise floor between two identical runs.
pub const FLIP_NOISE_FACTOR: f32 = 3.0;

/// Smallest FLIP mean threshold: bit-identical reruns have a zero noise floor, and a change
/// this small is invisible side by side.
pub const FLIP_FLOOR: f32 = 0.004;

/// Smallest FLIP p95 threshold (same reason as [`FLIP_FLOOR`]).
pub const FLIP_P95_FLOOR: f32 = 0.02;

/// A plate is flagged when its relative gap to the UE plate grows by more than this
/// (0.10 = ten percentage points of the UE value).
pub const UE_DRIFT_LIMIT: f32 = 0.10;

/// A plate is flagged when its hue gap to the UE plate grows by more than this many degrees:
/// ten per cent of the 60° between a primary and a secondary hue (the look-r2 mesa test's band is
/// 20–26° against UE 22–25°).
pub const HUE_DRIFT_LIMIT_DEG: f32 = 6.0;

/// A plate is flagged when its GPU frame p50 rises by more than this fraction.
pub const GPU_LIMIT: f32 = 0.10;

/// Fewer saturated pixels than this fraction of the frame leave the hue undefined.
pub const HUE_MIN_FRACTION: f32 = 0.01;

/// Look statistics of one displayed frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlateStats {
    /// Mean BT.709 luma of the 8-bit values (0–255).
    pub mean_luma: f32,
    /// Median luma (0–255).
    pub median_luma: f32,
    /// Standard deviation of the luma (0–255).
    pub contrast: f32,
    /// Mean HSV saturation (0–1).
    pub saturation: f32,
    /// Circular mean hue of the saturated pixels, degrees; `None` when fewer than
    /// [`HUE_MIN_FRACTION`] of the pixels are saturated.
    pub hue: Option<f32>,
    /// Fraction of the pixels in the hue mask.
    pub saturated_fraction: f32,
    /// Per cent of pixels with any channel ≥ 250.
    pub clipped_pct: f32,
}

/// Measures an RGBA8 frame.
#[must_use]
pub fn plate_stats(rgba: &[u8], width: u32, height: u32) -> PlateStats {
    let n = (width as usize * height as usize).min(rgba.len() / 4);
    if n == 0 {
        return PlateStats {
            mean_luma: 0.0,
            median_luma: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            hue: None,
            saturated_fraction: 0.0,
            clipped_pct: 0.0,
        };
    }
    let mut lumas = Vec::with_capacity(n);
    let (mut sum, mut sum_sq, mut sat_sum) = (0.0f64, 0.0f64, 0.0f64);
    let (mut hx, mut hy, mut hue_n, mut clipped) = (0.0f64, 0.0f64, 0usize, 0usize);
    for px in rgba.as_chunks::<4>().0.iter().take(n) {
        let y = crate::look_r2::luma(px.as_slice());
        lumas.push(y);
        sum += f64::from(y);
        sum_sq += f64::from(y) * f64::from(y);
        if px[..3].iter().any(|&c| c >= 250) {
            clipped += 1;
        }
        let (r, g, b) = (
            f32::from(px[0]) / 255.0,
            f32::from(px[1]) / 255.0,
            f32::from(px[2]) / 255.0,
        );
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let s = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
        sat_sum += f64::from(s);
        if mx >= 0.12 && s >= 0.35 {
            let d = mx - mn;
            let h = if mx == r {
                ((g - b) / d).rem_euclid(6.0)
            } else if mx == g {
                (b - r) / d + 2.0
            } else {
                (r - g) / d + 4.0
            } * 60.0;
            let a = f64::from(h).to_radians();
            hx += a.cos();
            hy += a.sin();
            hue_n += 1;
        }
    }
    let count = n as f64;
    let mean = sum / count;
    lumas.sort_by(f32::total_cmp);
    let saturated_fraction = (hue_n as f64 / count) as f32;
    PlateStats {
        mean_luma: mean as f32,
        median_luma: lumas[n / 2],
        contrast: (sum_sq / count - mean * mean).max(0.0).sqrt() as f32,
        saturation: (sat_sum / count) as f32,
        hue: (saturated_fraction >= HUE_MIN_FRACTION)
            .then(|| hy.atan2(hx).to_degrees().rem_euclid(360.0) as f32),
        saturated_fraction,
        clipped_pct: (clipped as f64 / count * 100.0) as f32,
    }
}

/// How far a plate sits from the UE plate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UeGap {
    /// `|luma − UE| / UE` (mean luma).
    pub luma: f32,
    /// `|contrast − UE| / UE`.
    pub contrast: f32,
    /// `|saturation − UE| / UE`.
    pub saturation: f32,
    /// Circular hue distance, degrees; `None` when either hue is undefined.
    pub hue_deg: Option<f32>,
}

fn relative_gap(value: f32, reference: f32) -> f32 {
    (value - reference).abs() / reference.abs().max(1e-3)
}

/// The gap from `this` to the UE plate's statistics.
#[must_use]
pub fn ue_gap(this: &PlateStats, ue: &PlateStats) -> UeGap {
    UeGap {
        luma: relative_gap(this.mean_luma, ue.mean_luma),
        contrast: relative_gap(this.contrast, ue.contrast),
        saturation: relative_gap(this.saturation, ue.saturation),
        hue_deg: this.hue.zip(ue.hue).map(|(a, b)| {
            let d = (a - b).rem_euclid(360.0);
            d.min(360.0 - d)
        }),
    }
}

/// The UE-match measures that moved away from UE by more than the limits, as readable notes
/// (`"luma gap 4 % → 17 %"`).
#[must_use]
pub fn ue_drift(prev: &UeGap, this: &UeGap) -> Vec<String> {
    let mut notes = Vec::new();
    for (name, p, t) in [
        ("luma", prev.luma, this.luma),
        ("contrast", prev.contrast, this.contrast),
        ("saturation", prev.saturation, this.saturation),
    ] {
        if t - p > UE_DRIFT_LIMIT {
            notes.push(format!(
                "{name} gap to UE {:.0} % → {:.0} %",
                p * 100.0,
                t * 100.0
            ));
        }
    }
    if let (Some(p), Some(t)) = (prev.hue_deg, this.hue_deg)
        && t - p > HUE_DRIFT_LIMIT_DEG
    {
        notes.push(format!("hue gap to UE {p:.1}° → {t:.1}°"));
    }
    notes
}

/// GPU frame p50 (ms) per plate from a `do27_chase --csv` file (columns found by header name,
/// so older and newer CSV layouts both parse). Rows with a zero GPU time (no timestamps) are
/// skipped.
#[must_use]
pub fn gpu_p50_by_plate(csv: &str) -> BTreeMap<String, f32> {
    let mut lines = csv.lines();
    let Some(header) = lines.next() else {
        return BTreeMap::new();
    };
    let cols: Vec<&str> = header.split(',').map(str::trim).collect();
    let (Some(map_col), Some(gpu_col)) = (
        cols.iter().position(|c| *c == "map"),
        cols.iter().position(|c| *c == "gpu_frame"),
    ) else {
        return BTreeMap::new();
    };
    let mut samples: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        let (Some(map), Some(Ok(gpu))) = (
            fields.get(map_col),
            fields.get(gpu_col).map(|v| v.trim().parse::<f32>()),
        ) else {
            continue;
        };
        if gpu > 0.0 {
            samples.entry(map.trim().to_string()).or_default().push(gpu);
        }
    }
    samples
        .into_iter()
        .map(|(plate, mut v)| {
            v.sort_by(f32::total_cmp);
            (plate, v[v.len() / 2])
        })
        .collect()
}

/// FLIP between two renders of the same tree, per `tier/shot`, plus the GPU p50 spread
/// between their timing runs: what [`flip_thresholds`] scales.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NoiseFloor {
    /// Tag (or tree) both runs rendered.
    pub source: String,
    /// `tier/shot` → (FLIP mean, FLIP p95) between the two runs.
    pub flip: BTreeMap<String, (f32, f32)>,
    /// Plate → relative GPU frame p50 difference between the two timing runs.
    pub gpu: BTreeMap<String, f32>,
}

/// `(mean, p95)` FLIP thresholds for `tier/shot`: [`FLIP_NOISE_FACTOR`] × its noise floor, at
/// least [`FLIP_FLOOR`] / [`FLIP_P95_FLOOR`].
#[must_use]
pub fn flip_thresholds(noise: Option<&NoiseFloor>, key: &str) -> (f32, f32) {
    let (mean, p95) = noise.and_then(|n| n.flip.get(key)).copied().unwrap_or((0.0, 0.0));
    (
        (mean * FLIP_NOISE_FACTOR).max(FLIP_FLOOR),
        (p95 * FLIP_NOISE_FACTOR).max(FLIP_P95_FLOOR),
    )
}

/// One plate of one tier in a tag-to-tag comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlateComparison {
    /// Tier directory name.
    pub tier: String,
    /// Shot name.
    pub shot: String,
    /// FLIP mean against the previous tag (`None` without a previous render).
    pub flip_mean: Option<f32>,
    /// FLIP p95 against the previous tag.
    pub flip_p95: Option<f32>,
    /// SSIM against the previous tag.
    pub ssim: Option<f32>,
    /// `(mean, p95)` thresholds used.
    pub flip_threshold: (f32, f32),
    /// This tag's statistics.
    pub this: PlateStats,
    /// The previous tag's statistics.
    pub prev: Option<PlateStats>,
    /// The UE plate's statistics at the render size.
    pub ue: Option<PlateStats>,
    /// Flags raised (empty: nothing to look at).
    pub flags: Vec<String>,
}

impl PlateComparison {
    /// Compares one plate. `diff` is `(flip mean, flip p95, ssim)` against the previous tag.
    #[must_use]
    pub fn new(
        tier: &str,
        shot: &str,
        this: PlateStats,
        prev: Option<PlateStats>,
        ue: Option<PlateStats>,
        diff: Option<(f32, f32, f32)>,
        noise: Option<&NoiseFloor>,
    ) -> Self {
        let flip_threshold = flip_thresholds(noise, &format!("{tier}/{shot}"));
        let mut flags = Vec::new();
        if let Some((mean, p95, _)) = diff
            && (mean > flip_threshold.0 || p95 > flip_threshold.1)
        {
            flags.push(format!(
                "frame changed: FLIP {mean:.4} (limit {:.4}), p95 {p95:.3} (limit {:.3})",
                flip_threshold.0, flip_threshold.1
            ));
        }
        if let (Some(prev), Some(ue)) = (prev, ue) {
            flags.extend(ue_drift(&ue_gap(&prev, &ue), &ue_gap(&this, &ue)));
        }
        Self {
            tier: tier.to_string(),
            shot: shot.to_string(),
            flip_mean: diff.map(|d| d.0),
            flip_p95: diff.map(|d| d.1),
            ssim: diff.map(|d| d.2),
            flip_threshold,
            this,
            prev,
            ue,
            flags,
        }
    }
}

/// GPU frame p50 of one plate in both tags.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuComparison {
    /// Plate name (`do27_chase` map column).
    pub plate: String,
    /// Previous tag p50, ms.
    pub prev_ms: Option<f32>,
    /// This tag p50, ms.
    pub this_ms: Option<f32>,
    /// Raised when this is more than [`GPU_LIMIT`] above the previous tag.
    pub flagged: bool,
}

/// Pairs the plates of two timing runs.
#[must_use]
pub fn compare_gpu(prev: &BTreeMap<String, f32>, this: &BTreeMap<String, f32>) -> Vec<GpuComparison> {
    let mut plates: Vec<&String> = prev.keys().chain(this.keys()).collect();
    plates.sort();
    plates.dedup();
    plates
        .into_iter()
        .map(|plate| {
            let (p, t) = (prev.get(plate).copied(), this.get(plate).copied());
            GpuComparison {
                plate: plate.clone(),
                prev_ms: p,
                this_ms: t,
                flagged: matches!((p, t), (Some(p), Some(t)) if p > 0.0 && t > p * (1.0 + GPU_LIMIT)),
            }
        })
        .collect()
}

/// Everything one comparison found (`compare.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompareSummary {
    /// This tag's label.
    pub this: String,
    /// The previous tag's label (`None` on the first run).
    pub prev: Option<String>,
    /// Where the renders, sheets and heat maps live.
    pub location: String,
    /// Fingerprint of the DO27 content snapshot both sets were rendered on. DO27's content changes
    /// between tags too; comparing sets rendered on different content would blame the engine for it.
    #[serde(default)]
    pub content: Option<String>,
    /// Per tier and plate.
    pub plates: Vec<PlateComparison>,
    /// GPU frame p50 per timing plate.
    pub gpu: Vec<GpuComparison>,
}

impl CompareSummary {
    /// Number of flags raised.
    #[must_use]
    pub fn flag_count(&self) -> usize {
        self.plates.iter().map(|p| p.flags.len()).sum::<usize>()
            + self.gpu.iter().filter(|g| g.flagged).count()
    }

    /// The short markdown summary the orchestrator reads.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut md = String::new();
        let prev = self.prev.as_deref().unwrap_or("(none)");
        let _ = writeln!(md, "# look-ci {} vs {prev}\n", self.this);
        let _ = writeln!(
            md,
            "Renders, `UE | prev | this | FLIP` sheets and heat maps: `{}` (`compare/<tier>/`, `overview_<tier>.png`).\n",
            self.location
        );
        if let Some(content) = &self.content {
            let _ = writeln!(
                md,
                "DO27 content snapshot `{content}`: both sets are rendered on it, so a flag is an engine change.\n"
            );
        }
        let _ = writeln!(md, "**Flags: {}**\n", self.flag_count());
        for p in &self.plates {
            for f in &p.flags {
                let _ = writeln!(md, "- `{}/{}`: {f}", p.tier, p.shot);
            }
        }
        for g in self.gpu.iter().filter(|g| g.flagged) {
            if let (Some(p), Some(t)) = (g.prev_ms, g.this_ms) {
                let _ = writeln!(
                    md,
                    "- GPU `{}`: p50 {p:.2} → {t:.2} ms ({:+.1} %)",
                    g.plate,
                    (t / p - 1.0) * 100.0
                );
            }
        }
        let fmt = |v: Option<f32>, digits: usize| v.map_or("–".to_string(), |v| format!("{v:.digits$}"));
        let mut tiers: Vec<&str> = self.plates.iter().map(|p| p.tier.as_str()).collect();
        tiers.dedup();
        for tier in tiers {
            let _ = writeln!(
                md,
                "\n## {tier}\n\n| plate | FLIP | p95 | SSIM | luma prev→this (UE) | contrast (UE) | hue (UE) | flag |\n|---|---|---|---|---|---|---|---|"
            );
            for p in self.plates.iter().filter(|p| p.tier == tier) {
                let ue = p.ue.as_ref();
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {} | {}→{:.0} ({}) | {:.0} ({}) | {} ({}) | {} |",
                    p.shot,
                    fmt(p.flip_mean, 4),
                    fmt(p.flip_p95, 3),
                    fmt(p.ssim, 3),
                    fmt(p.prev.map(|s| s.mean_luma), 0),
                    p.this.mean_luma,
                    fmt(ue.map(|s| s.mean_luma), 0),
                    p.this.contrast,
                    fmt(ue.map(|s| s.contrast), 0),
                    fmt(p.this.hue, 0),
                    fmt(ue.and_then(|s| s.hue), 0),
                    if p.flags.is_empty() { "" } else { "**FLAG**" },
                );
            }
        }
        if !self.gpu.is_empty() {
            let _ = writeln!(
                md,
                "\n## GPU frame p50 (`do27_chase --plates`, bench 5090)\n\n| plate | prev ms | this ms | Δ | flag |\n|---|---|---|---|---|"
            );
            for g in &self.gpu {
                let delta = match (g.prev_ms, g.this_ms) {
                    (Some(p), Some(t)) if p > 0.0 => format!("{:+.1} %", (t / p - 1.0) * 100.0),
                    _ => "–".to_string(),
                };
                let _ = writeln!(
                    md,
                    "| {} | {} | {} | {delta} | {} |",
                    g.plate,
                    fmt(g.prev_ms, 2),
                    fmt(g.this_ms, 2),
                    if g.flagged { "**FLAG**" } else { "" }
                );
            }
        }
        md
    }
}

/// One plate in a render set's `metrics.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlateRecord {
    /// Shot name.
    pub shot: String,
    /// The render's statistics.
    pub stats: PlateStats,
    /// The UE plate's statistics at the render size, when the UE plates were given.
    pub ue: Option<PlateStats>,
    /// The gap to the UE plate.
    pub ue_gap: Option<UeGap>,
}

/// `metrics.json` of one tier's render set (`look_ci render` / `look_ci metrics`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsFile {
    /// Tier (`LOOK_R2_TIER`, empty = Desktop High).
    pub tier: String,
    /// Plates found, in parity order.
    pub plates: Vec<PlateRecord>,
}

/// Parity plates in sheet order with their file stems.
#[must_use]
pub fn plate_names() -> Vec<&'static str> {
    R2Shot::PARITY.iter().map(|s| s.name()).collect()
}

/// Places equally sized RGBA8 panels left to right with a `gap`-pixel white divider.
#[must_use]
pub fn hstack(panels: &[&[u8]], width: u32, height: u32, gap: u32) -> (u32, Vec<u8>) {
    let count = panels.len() as u32;
    let out_w = (width * count + gap * count.saturating_sub(1)).max(1);
    let mut out = vec![255u8; (out_w * height * 4) as usize];
    let row_bytes = (width * 4) as usize;
    for (i, panel) in panels.iter().enumerate() {
        let x0 = i as u32 * (width + gap);
        for y in 0..height {
            let src = (y * width * 4) as usize;
            let dst = ((y * out_w + x0) * 4) as usize;
            if let (Some(s), Some(d)) = (panel.get(src..src + row_bytes), out.get_mut(dst..dst + row_bytes)) {
                d.copy_from_slice(s);
            }
        }
    }
    (out_w, out)
}

/// Stacks equally sized RGBA8 rows top to bottom with a `gap`-pixel white divider.
#[must_use]
pub fn vstack(rows: &[Vec<u8>], width: u32, height: u32, gap: u32) -> (u32, Vec<u8>) {
    let count = rows.len() as u32;
    let out_h = (height * count + gap * count.saturating_sub(1)).max(1);
    let mut out = vec![255u8; (width * out_h * 4) as usize];
    let bytes = (width * height * 4) as usize;
    for (i, row) in rows.iter().enumerate() {
        let dst = (i as u32 * (height + gap) * width * 4) as usize;
        if let (Some(s), Some(d)) = (row.get(..bytes), out.get_mut(dst..dst + bytes)) {
            d.copy_from_slice(s);
        }
    }
    (out_h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: u32, h: u32, c: [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|_| [c[0], c[1], c[2], 255]).collect()
    }

    #[test]
    fn stats_of_a_flat_red_brown_frame() {
        // A Mars mesa colour: hue 60·(g−b)/(r−b) = 60·(80−40)/(160−40) = 20°.
        let s = plate_stats(&flat(8, 4, [160, 80, 40]), 8, 4);
        let y = 0.2126 * 160.0 + 0.7152 * 80.0 + 0.0722 * 40.0;
        assert!((s.mean_luma - y).abs() < 1e-3 && (s.median_luma - y).abs() < 1e-3);
        assert!(s.contrast < 1e-3);
        assert!((s.saturation - 0.75).abs() < 1e-4);
        assert!(s.hue.is_some_and(|h| (h - 20.0).abs() < 1e-3), "{:?}", s.hue);
        assert_eq!(s.clipped_pct, 0.0);
    }

    #[test]
    fn grey_frames_have_no_hue_and_clipping_counts() {
        let mut px = flat(10, 10, [128, 128, 128]);
        px[..4].copy_from_slice(&[255, 255, 255, 255]);
        let s = plate_stats(&px, 10, 10);
        assert_eq!(s.hue, None);
        assert!((s.clipped_pct - 1.0).abs() < 1e-4);
        assert!(s.contrast > 0.0);
        assert_eq!(plate_stats(&[], 0, 0).mean_luma, 0.0);
    }

    #[test]
    fn hue_mean_is_circular() {
        // Half the pixels at 350°, half at 10°: the mean is 0°, not 180°.
        let mut px = flat(2, 1, [255, 0, 43]);
        px[4..8].copy_from_slice(&[255, 43, 0, 255]);
        let h = plate_stats(&px, 2, 1).hue.unwrap_or(f32::NAN);
        assert!(!(1.0..=359.0).contains(&h), "{h}");
    }

    #[test]
    fn ue_drift_flags_only_a_gap_that_grows_past_the_limit() {
        let ue = plate_stats(&flat(4, 4, [160, 80, 40]), 4, 4);
        let near = plate_stats(&flat(4, 4, [165, 82, 41]), 4, 4);
        let far = plate_stats(&flat(4, 4, [200, 100, 50]), 4, 4);
        let olive = plate_stats(&flat(4, 4, [160, 110, 40]), 4, 4);
        assert!(ue_drift(&ue_gap(&near, &ue), &ue_gap(&near, &ue)).is_empty());
        // Moving back toward UE is never a flag.
        assert!(ue_drift(&ue_gap(&far, &ue), &ue_gap(&near, &ue)).is_empty());
        let notes = ue_drift(&ue_gap(&near, &ue), &ue_gap(&far, &ue));
        assert!(notes.iter().any(|n| n.starts_with("luma")), "{notes:?}");
        let notes = ue_drift(&ue_gap(&near, &ue), &ue_gap(&olive, &ue));
        assert!(notes.iter().any(|n| n.starts_with("hue")), "{notes:?}");
    }

    #[test]
    fn timing_csv_p50_by_plate_ignores_zero_and_unknown_columns() {
        let csv = "map,frame,wall,gpu_frame,extra\n\
                   a,0,1,2.0,9\na,1,1,4.0,9\na,2,1,3.0,9\na,3,1,0.0,9\nb,0,1,1.5,9\nbad line\n";
        let p = gpu_p50_by_plate(csv);
        assert_eq!(p.get("a"), Some(&3.0));
        assert_eq!(p.get("b"), Some(&1.5));
        assert!(gpu_p50_by_plate("map,frame\na,0\n").is_empty());
        let mut prev = BTreeMap::new();
        prev.insert("a".to_string(), 2.0);
        prev.insert("b".to_string(), 1.5);
        let g = compare_gpu(&prev, &p);
        assert!(g.iter().find(|g| g.plate == "a").is_some_and(|g| g.flagged));
        assert!(g.iter().find(|g| g.plate == "b").is_some_and(|g| !g.flagged));
    }

    #[test]
    fn flip_threshold_is_three_noise_floors_with_a_floor() {
        let mut noise = NoiseFloor::default();
        noise.flip.insert("deck/cockpit".into(), (0.01, 0.05));
        let (mean, p95) = flip_thresholds(Some(&noise), "deck/cockpit");
        assert!((mean - 0.03).abs() < 1e-6 && (p95 - 0.15).abs() < 1e-6);
        assert_eq!(
            flip_thresholds(Some(&noise), "deck/other"),
            (FLIP_FLOOR, FLIP_P95_FLOOR)
        );
        assert_eq!(flip_thresholds(None, "x"), (FLIP_FLOOR, FLIP_P95_FLOOR));
        let s = plate_stats(&flat(2, 2, [90, 90, 90]), 2, 2);
        let quiet = PlateComparison::new(
            "deck",
            "cockpit",
            s,
            Some(s),
            None,
            Some((0.02, 0.1, 0.99)),
            Some(&noise),
        );
        assert!(quiet.flags.is_empty());
        let loud = PlateComparison::new(
            "deck",
            "cockpit",
            s,
            Some(s),
            None,
            Some((0.05, 0.1, 0.9)),
            Some(&noise),
        );
        assert_eq!(loud.flags.len(), 1);
        let summary = CompareSummary {
            this: "t".into(),
            prev: Some("p".into()),
            location: "/x".into(),
            content: Some("0123abcd4567".into()),
            plates: vec![quiet, loud],
            gpu: Vec::new(),
        };
        assert_eq!(summary.flag_count(), 1);
        let md = summary.markdown();
        assert!(
            md.contains("**Flags: 1**")
                && md.contains("`deck/cockpit`: frame changed")
                && md.contains("snapshot `0123abcd4567`"),
            "{md}"
        );
    }

    #[test]
    fn stacks_place_panels_with_white_gaps() {
        let a = flat(2, 2, [10, 10, 10]);
        let b = flat(2, 2, [20, 20, 20]);
        let (w, px) = hstack(&[&a, &b], 2, 2, 1);
        assert_eq!(w, 5);
        assert_eq!(&px[0..4], &[10, 10, 10, 255]);
        assert_eq!(&px[8..12], &[255, 255, 255, 255]);
        assert_eq!(&px[12..16], &[20, 20, 20, 255]);
        let (h, px) = vstack(&[a, b], 2, 2, 1);
        assert_eq!(h, 5);
        assert_eq!(&px[(2 * 2 * 4)..(2 * 2 * 4 + 4)], &[255, 255, 255, 255]);
        assert_eq!(&px[(3 * 2 * 4)..(3 * 2 * 4 + 4)], &[20, 20, 20, 255]);
    }
}
