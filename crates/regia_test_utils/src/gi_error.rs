// SPDX-License-Identifier: MPL-2.0
//! Error of a real-time GI technique against the path tracer's indirect light (`specs/27` § 2.1,
//! § 2.2 GI rows; ADR-423).
//!
//! The inputs are linear radiance planes (cd/m², row-major, top row first): the reference is the
//! path tracer's indirect light (`indirect_diffuse + indirect_specular`), the test is the
//! real-time frame's indirect light (the full frame minus a direct-only frame). Three numbers
//! describe the error, each over the pixels that see a surface:
//!
//! - **RMSE**: per-channel root mean square difference, also relative to the scale (the mean
//!   reference luminance unless the caller gives one);
//! - **shadow bias**: the signed difference of the mean luminance in sun shadow, relative to the
//!   scale; where only indirect light reaches, this is the GI's own error;
//! - **energy ratio**: total test luminance over total reference luminance, 1 when the technique
//!   neither adds nor loses indirect light overall.
//!
//! Mean FLIP over the same pixels (`regia_render::compare`) is reported beside them.
//!
//! Recorded values ([`GiRecord`]) live in plain text files, one per scene, under
//! [`gi_thresholds_dir`]; a test fails when a technique's error grows past its record by more
//! than a [`GiTolerance`] ([`regressions`]). A run with [`GI_RECORD_ENV`]`=1` writes the measured
//! values instead, like `REGIA_BLESS=1` for goldens.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Environment variable of the record mode: with `1`, GI validation tests write the measured
/// errors to their record files instead of checking them.
pub const GI_RECORD_ENV: &str = "REGIA_GI_RECORD";

/// Environment variable naming a directory where GI validation tests write their error tables
/// (`<scene>.md`) and side-by-side images for review.
pub const GI_TABLE_ENV: &str = "REGIA_GI_TABLE";

/// `true` when [`GI_RECORD_ENV`] is `1`.
#[must_use]
pub fn gi_record_mode() -> bool {
    regia_core::brand::env_var(GI_RECORD_ENV).as_deref() == Ok("1")
}

/// Rec. 709 luminance of a linear RGB value.
#[must_use]
pub fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Sum of two radiance planes (for example the tracer's diffuse and specular indirect AOVs).
#[must_use]
pub fn add_planes(a: &[[f32; 3]], b: &[[f32; 3]]) -> Vec<[f32; 3]> {
    a.iter()
        .zip(b)
        .map(|(x, y)| [x[0] + y[0], x[1] + y[1], x[2] + y[2]])
        .collect()
}

/// `full − direct`, signed (not clamped at zero, so noise and over-darkening both count).
#[must_use]
pub fn sub_planes(full: &[[f32; 3]], direct: &[[f32; 3]]) -> Vec<[f32; 3]> {
    full.iter()
        .zip(direct)
        .map(|(f, d)| [f[0] - d[0], f[1] - d[1], f[2] - d[2]])
        .collect()
}

/// Pixels that see a surface, away from silhouettes: the pixel and its four neighbours have a
/// positive depth (the tracer's depth AOV is 0 on the sky), and each neighbour's depth is within
/// `max_step` (a fraction of the pixel's depth) of it. The image border is excluded.
#[must_use]
pub fn surface_mask(depth: &[f32], width: u32, height: u32, max_step: f32) -> Vec<bool> {
    let (w, h) = (width as usize, height as usize);
    let mut mask = vec![false; depth.len()];
    if depth.len() != w * h || w < 3 || h < 3 {
        return mask;
    }
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let d = depth[i];
            if d <= 0.0 || !d.is_finite() {
                continue;
            }
            mask[i] = [i - 1, i + 1, i - w, i + w].iter().all(|&j| {
                let n = depth[j];
                n > 0.0 && n.is_finite() && (n - d).abs() <= max_step * d
            });
        }
    }
    mask
}

/// The `q` quantile (0..=1) of `values` (nearest rank); 0 when empty.
#[must_use]
pub fn quantile(values: &[f32], q: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let k = ((sorted.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[k.min(sorted.len() - 1)]
}

/// Surface pixels in sun shadow: the tracer's sun visibility is below one half, or the direct
/// light there is below `lit_fraction` of the 90th percentile of the direct light over the
/// surface (faces turned away from the sun, which the visibility AOV may still call lit).
#[must_use]
pub fn shadow_mask(
    surface: &[bool],
    sun_visibility: &[f32],
    direct: &[[f32; 3]],
    lit_fraction: f32,
) -> Vec<bool> {
    let lit: Vec<f32> = surface
        .iter()
        .zip(direct)
        .filter(|(s, _)| **s)
        .map(|(_, c)| luminance(*c))
        .collect();
    let level = quantile(&lit, 0.9);
    surface
        .iter()
        .zip(sun_visibility)
        .zip(direct)
        .map(|((s, v), c)| *s && (*v < 0.5 || luminance(*c) <= lit_fraction * level))
        .collect()
}

/// One comparison: a test plane against a reference plane over a surface mask.
#[derive(Clone, Copy, Debug)]
pub struct GiErrorInput<'a> {
    /// Reference radiance (the path tracer's indirect light).
    pub reference: &'a [[f32; 3]],
    /// Tested radiance (the real-time frame's indirect light).
    pub test: &'a [[f32; 3]],
    /// Pixels scored ([`surface_mask`]).
    pub surface: &'a [bool],
    /// Pixels in sun shadow ([`shadow_mask`]), a subset of `surface`.
    pub shadow: &'a [bool],
    /// Image width (for FLIP).
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// Exposure of the FLIP display transform.
    pub ev100: f32,
    /// Luminance (cd/m²) every relative number divides by. `None`: the mean reference luminance
    /// over `surface` (and over `shadow` for the shadow bias). A scene whose reference indirect
    /// light is about zero (a sealed room) passes the lit exterior's level instead, so the
    /// relative errors read as a leak fraction.
    pub scale: Option<f32>,
}

/// Error of one technique against the reference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GiError {
    /// Pixels scored.
    pub pixels: usize,
    /// Of those, pixels in sun shadow.
    pub shadow_pixels: usize,
    /// The luminance relative numbers divide by.
    pub scale: f32,
    /// Mean reference luminance over the surface.
    pub mean_reference: f32,
    /// Mean test luminance over the surface.
    pub mean_test: f32,
    /// Per-channel RMSE over the surface, cd/m².
    pub rmse: f32,
    /// [`Self::rmse`] over [`Self::scale`].
    pub rel_rmse: f32,
    /// `(mean test − mean reference)` luminance in sun shadow over the shadow scale; `None`
    /// without shadow pixels.
    pub shadow_bias: Option<f32>,
    /// Total test luminance over total reference luminance on the surface; `None` when the
    /// reference has no indirect light there.
    pub energy_ratio: Option<f32>,
    /// Mean FLIP over the surface (both planes through the runtime's display transform).
    pub flip_mean: f32,
    /// 95th percentile (nearest rank, [`quantile`]) of the per-pixel FLIP over the surface: the
    /// `specs/27` § 2.2 GI p95 row (ADR-2951).
    pub flip_p95: f32,
}

/// Masked mean of `f` over the pixels where `mask` is set; `(mean, count)`.
fn masked_mean(values: &[[f32; 3]], mask: &[bool], f: impl Fn([f32; 3]) -> f64) -> (f64, usize) {
    let (sum, n) = values
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .fold((0.0f64, 0usize), |(s, n), (c, _)| (s + f(*c), n + 1));
    (if n > 0 { sum / n as f64 } else { 0.0 }, n)
}

/// Smallest luminance treated as light (below it a reference counts as dark).
const DARK: f64 = 1e-6;

/// Mean FLIP of `test` against `reference` over `mask`; outside it both read the reference, so
/// only the masked pixels can differ.
#[must_use]
pub fn masked_flip(
    reference: &[[f32; 3]],
    test: &[[f32; 3]],
    mask: &[bool],
    width: u32,
    height: u32,
    ev100: f32,
) -> f32 {
    masked_flip_stats(reference, test, mask, width, height, ev100).0
}

/// [`masked_flip`]'s mean and the 95th percentile ([`quantile`], nearest rank) of the masked
/// per-pixel FLIP: `(mean, p95)`; `(0, 0)` with no masked pixel.
#[must_use]
pub fn masked_flip_stats(
    reference: &[[f32; 3]],
    test: &[[f32; 3]],
    mask: &[bool],
    width: u32,
    height: u32,
    ev100: f32,
) -> (f32, f32) {
    let blended: Vec<[f32; 3]> = test
        .iter()
        .zip(reference)
        .zip(mask)
        .map(|((t, r), m)| if *m { *t } else { *r })
        .collect();
    let (_, flip) = regia_render::compare::compare_linear(
        reference,
        &blended,
        width,
        height,
        ev100,
        regia_render::compare::DEFAULT_PPD,
    );
    let masked: Vec<f32> = flip
        .map
        .iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .map(|(v, _)| *v)
        .collect();
    let sum: f64 = masked.iter().map(|v| f64::from(*v)).sum();
    let mean = (sum / masked.len().max(1) as f64) as f32;
    (mean, quantile(&masked, 0.95))
}

/// Scores one comparison. Planes and masks of different lengths score nothing (zero pixels,
/// infinite RMSE).
#[must_use]
pub fn measure(input: &GiErrorInput<'_>) -> GiError {
    let n = input.reference.len();
    let consistent = input.test.len() == n && input.surface.len() == n && input.shadow.len() == n && n > 0;
    if !consistent {
        return GiError {
            pixels: 0,
            shadow_pixels: 0,
            scale: 0.0,
            mean_reference: 0.0,
            mean_test: 0.0,
            rmse: f32::INFINITY,
            rel_rmse: f32::INFINITY,
            shadow_bias: None,
            energy_ratio: None,
            flip_mean: 1.0,
            flip_p95: 1.0,
        };
    }
    let lum = |c: [f32; 3]| f64::from(luminance(c));
    let (mean_ref, pixels) = masked_mean(input.reference, input.surface, lum);
    let (mean_test, _) = masked_mean(input.test, input.surface, lum);
    let se = input
        .reference
        .iter()
        .zip(input.test)
        .zip(input.surface)
        .filter(|(_, m)| **m)
        .fold(0.0f64, |acc, ((r, t), _)| {
            acc + (0..3)
                .map(|c| {
                    let d = f64::from(t[c] - r[c]);
                    d * d
                })
                .sum::<f64>()
        });
    let rmse = if pixels > 0 {
        (se / (3 * pixels) as f64).sqrt()
    } else {
        f64::INFINITY
    };
    let scale = input.scale.map_or(mean_ref, f64::from);
    let rel = |v: f64, by: f64| if by > DARK { v / by } else { f64::INFINITY };
    let (shadow_ref, shadow_pixels) = masked_mean(input.reference, input.shadow, lum);
    let (shadow_test, _) = masked_mean(input.test, input.shadow, lum);
    let shadow_scale = input.scale.map_or(shadow_ref, f64::from);
    let shadow_bias = (shadow_pixels > 0 && shadow_scale > DARK)
        .then_some(((shadow_test - shadow_ref) / shadow_scale) as f32);
    let energy_ratio = (mean_ref > DARK).then_some((mean_test / mean_ref) as f32);
    let (flip_mean, flip_p95) = masked_flip_stats(
        input.reference,
        input.test,
        input.surface,
        input.width,
        input.height,
        input.ev100,
    );
    GiError {
        pixels,
        shadow_pixels,
        scale: scale as f32,
        mean_reference: mean_ref as f32,
        mean_test: mean_test as f32,
        rmse: rmse as f32,
        rel_rmse: rel(rmse, scale) as f32,
        shadow_bias,
        energy_ratio,
        flip_mean,
        flip_p95,
    }
}

/// The recorded error of one technique on one scene: the values a regression is measured from.
#[derive(Clone, Debug, PartialEq)]
pub struct GiRecord {
    /// Scene name (`[a-z0-9_]`).
    pub scene: String,
    /// Technique name (`[a-z0-9_]`).
    pub technique: String,
    /// Recorded [`GiError::rel_rmse`].
    pub rel_rmse: f32,
    /// Recorded [`GiError::shadow_bias`].
    pub shadow_bias: Option<f32>,
    /// Recorded [`GiError::energy_ratio`].
    pub energy_ratio: Option<f32>,
}

impl GiRecord {
    /// The record of a measurement.
    #[must_use]
    pub fn from_error(scene: &str, technique: &str, error: &GiError) -> Self {
        Self {
            scene: scene.to_owned(),
            technique: technique.to_owned(),
            rel_rmse: error.rel_rmse,
            shadow_bias: error.shadow_bias,
            energy_ratio: error.energy_ratio,
        }
    }
}

/// How far a measurement may drift past its record before it is a regression: the allowed
/// magnitude is `recorded · (1 + relative) + absolute`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GiTolerance {
    /// Allowed growth as a fraction of the recorded magnitude.
    pub relative: f32,
    /// Allowed growth on top, in the metric's own (relative) units.
    pub absolute: f32,
}

impl GiTolerance {
    /// The adapter the records are taken on (lavapipe, the gate's reference). Both renderers are
    /// deterministic there for a fixed seed and frame count, so this only absorbs driver and
    /// compiler drift between LLVM builds.
    pub const REFERENCE: Self = Self {
        relative: 0.10,
        absolute: 0.02,
    };

    /// Any adapter that is not lavapipe (hardware GPUs, and WARP, which `golden_icd` classes as
    /// other software): records are taken on lavapipe, and a hardware GPU
    /// differs in sampling precision, shadow filtering and probe ray rotation results, so only a
    /// gross regression fails there (the same policy as lavapipe-blessed goldens, ADR-025).
    pub const OTHER_ADAPTER: Self = Self {
        relative: 0.50,
        absolute: 0.10,
    };

    /// The tolerance for an adapter: [`Self::REFERENCE`] on lavapipe, else
    /// [`Self::OTHER_ADAPTER`].
    #[must_use]
    pub fn for_adapter(info: &wgpu::AdapterInfo) -> Self {
        match crate::golden::golden_icd(info) {
            crate::golden::GoldenIcd::Lavapipe { .. } => Self::REFERENCE,
            _ => Self::OTHER_ADAPTER,
        }
    }

    fn allows(self, recorded: f32, measured: f32) -> bool {
        measured.is_finite() && measured <= recorded.abs() * (1.0 + self.relative) + self.absolute
    }
}

/// The regressions of `measured` against `record`, one message each (empty: no regression).
///
/// - relative RMSE may not grow past the tolerance;
/// - the shadow bias's magnitude may not grow past it;
/// - the energy ratio's distance from 1 may not grow past it;
/// - a metric the record has may not disappear (and the other way round).
#[must_use]
pub fn regressions(measured: &GiError, record: &GiRecord, tolerance: GiTolerance) -> Vec<String> {
    let mut out = Vec::new();
    let what = format!("{} / {}", record.scene, record.technique);
    if !tolerance.allows(record.rel_rmse, measured.rel_rmse) {
        out.push(format!(
            "{what}: relative RMSE {:.4} exceeds the record {:.4} (+{:.0} % + {:.3})",
            measured.rel_rmse,
            record.rel_rmse,
            tolerance.relative * 100.0,
            tolerance.absolute
        ));
    }
    match (record.shadow_bias, measured.shadow_bias) {
        (Some(r), Some(m)) if !tolerance.allows(r, m.abs()) => out.push(format!(
            "{what}: shadow bias {m:+.4} is further from 0 than the record {r:+.4} allows"
        )),
        (Some(r), None) => out.push(format!(
            "{what}: the record has a shadow bias ({r:+.4}) but the frame has no shadow pixels"
        )),
        (None, Some(m)) => out.push(format!(
            "{what}: the frame has a shadow bias ({m:+.4}) the record does not; re-record"
        )),
        _ => {}
    }
    match (record.energy_ratio, measured.energy_ratio) {
        (Some(r), Some(m)) if !tolerance.allows(r - 1.0, m - 1.0) || !tolerance.allows(r - 1.0, 1.0 - m) => {
            out.push(format!(
                "{what}: energy ratio {m:.4} is further from 1 than the record {r:.4} allows"
            ))
        }
        (Some(r), None) => out.push(format!(
            "{what}: the record has an energy ratio ({r:.4}) but the reference is dark now"
        )),
        (None, Some(m)) => out.push(format!(
            "{what}: the frame has an energy ratio ({m:.4}) the record does not; re-record"
        )),
        _ => {}
    }
    out
}

/// Notes for a measurement clearly better than its record (beyond the tolerance), so the
/// record can be tightened by re-recording. Never a failure.
#[must_use]
pub fn improvements(measured: &GiError, record: &GiRecord, tolerance: GiTolerance) -> Vec<String> {
    let mut out = Vec::new();
    let what = format!("{} / {}", record.scene, record.technique);
    if measured.rel_rmse * (1.0 + tolerance.relative) + tolerance.absolute < record.rel_rmse {
        out.push(format!(
            "{what}: relative RMSE {:.4} is well below the record {:.4}; re-record to tighten it",
            measured.rel_rmse, record.rel_rmse
        ));
    }
    if let (Some(r), Some(m)) = (record.shadow_bias, measured.shadow_bias)
        && m.abs() * (1.0 + tolerance.relative) + tolerance.absolute < r.abs()
    {
        out.push(format!(
            "{what}: shadow bias {m:+.4} is well inside the record {r:+.4}; re-record to tighten it"
        ));
    }
    if let (Some(r), Some(m)) = (record.energy_ratio, measured.energy_ratio)
        && (m - 1.0).abs() * (1.0 + tolerance.relative) + tolerance.absolute < (r - 1.0).abs()
    {
        out.push(format!(
            "{what}: energy ratio {m:.4} is well inside the record {r:.4}; re-record to tighten it"
        ));
    }
    out
}

/// The directory of the record files (`<scene>.txt`), in this crate.
#[must_use]
pub fn gi_thresholds_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gi_thresholds")
}

/// The record file of `scene`.
#[must_use]
pub fn gi_thresholds_path(scene: &str) -> PathBuf {
    gi_thresholds_dir().join(format!("{scene}.txt"))
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn parse_optional(field: &str) -> Result<Option<f32>, String> {
    if field == "-" {
        return Ok(None);
    }
    field
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
        .map(Some)
        .ok_or_else(|| format!("`{field}` is not a finite number or `-`"))
}

/// Parses a record file: `#` comments and blank lines are skipped; every other line is
/// `scene technique rel_rmse shadow_bias energy_ratio`, whitespace separated, with `-` for a
/// missing optional value.
///
/// # Errors
///
/// A malformed line, with its line number.
pub fn parse_records(text: &str) -> Result<Vec<GiRecord>, String> {
    let mut out = Vec::new();
    for (k, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [scene, technique, rel_rmse, shadow_bias, energy_ratio] = fields[..] else {
            return Err(format!(
                "line {}: expected 5 fields, found {}",
                k + 1,
                fields.len()
            ));
        };
        if !valid_name(scene) || !valid_name(technique) {
            return Err(format!(
                "line {}: names are lowercase ascii, digits and `_`",
                k + 1
            ));
        }
        let rel_rmse = parse_optional(rel_rmse)
            .map_err(|e| format!("line {}: rel_rmse {e}", k + 1))?
            .ok_or_else(|| format!("line {}: rel_rmse is required", k + 1))?;
        out.push(GiRecord {
            scene: scene.to_owned(),
            technique: technique.to_owned(),
            rel_rmse,
            shadow_bias: parse_optional(shadow_bias)
                .map_err(|e| format!("line {}: shadow_bias {e}", k + 1))?,
            energy_ratio: parse_optional(energy_ratio)
                .map_err(|e| format!("line {}: energy_ratio {e}", k + 1))?,
        });
    }
    Ok(out)
}

/// Writes records in the [`parse_records`] format, under a header naming the adapter they were
/// measured on.
#[must_use]
pub fn format_records(records: &[GiRecord], adapter: &str) -> String {
    let opt = |v: Option<f32>| v.map_or_else(|| "-".to_owned(), |v| v.to_string());
    let mut s = String::new();
    s.push_str("# GI validation record (ADR-423): scene technique rel_rmse shadow_bias energy_ratio\n");
    let _ = writeln!(s, "# measured on: {}", adapter.replace('\n', " "));
    s.push_str("# Re-record with REGIA_GI_RECORD=1 on lavapipe; say why in the commit message.\n");
    for r in records {
        let _ = writeln!(
            s,
            "{} {} {} {} {}",
            r.scene,
            r.technique,
            r.rel_rmse,
            opt(r.shadow_bias),
            opt(r.energy_ratio)
        );
    }
    s
}

/// Reads the records of `path`.
///
/// # Errors
///
/// The file cannot be read or does not parse.
pub fn read_records(path: &Path) -> Result<Vec<GiRecord>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_records(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `records` to `path` atomically (tmp + rename).
///
/// # Errors
///
/// The directory cannot be created or the file cannot be written.
pub fn write_records(path: &Path, records: &[GiRecord], adapter: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    crate::temp::write_atomic(path, format_records(records, adapter).as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// One row of the error table.
#[derive(Clone, Debug)]
pub struct GiRow {
    /// Scene name.
    pub scene: String,
    /// Technique name.
    pub technique: String,
    /// Its error.
    pub error: GiError,
}

/// The error table as Markdown: one row per technique.
#[must_use]
pub fn render_table(rows: &[GiRow]) -> String {
    let opt = |v: Option<f32>, signed: bool| match v {
        Some(v) if signed => format!("{v:+.3}"),
        Some(v) => format!("{v:.3}"),
        None => "–".to_owned(),
    };
    let mut s = String::from(
        "| Scene | Technique | Pixels (shadow) | Mean ref / test (cd/m²) | RMSE (cd/m²) | Rel. RMSE | Shadow bias | Energy ratio | FLIP |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let e = &r.error;
        let _ = writeln!(
            s,
            "| {} | {} | {} ({}) | {:.2} / {:.2} | {:.3} | {:.3} | {} | {} | {:.3} |",
            r.scene,
            r.technique,
            e.pixels,
            e.shadow_pixels,
            e.mean_reference,
            e.mean_test,
            e.rmse,
            e.rel_rmse,
            opt(e.shadow_bias, true),
            opt(e.energy_ratio, false),
            e.flip_mean
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 16;
    const H: u32 = 12;

    fn plane(f: impl Fn(usize) -> [f32; 3]) -> Vec<[f32; 3]> {
        (0..(W * H) as usize).map(f).collect()
    }

    fn input<'a>(
        reference: &'a [[f32; 3]],
        test: &'a [[f32; 3]],
        surface: &'a [bool],
        shadow: &'a [bool],
    ) -> GiErrorInput<'a> {
        GiErrorInput {
            reference,
            test,
            surface,
            shadow,
            width: W,
            height: H,
            ev100: 8.0,
            scale: None,
        }
    }

    #[test]
    fn identical_planes_have_no_error() {
        let r = plane(|i| [100.0 + i as f32, 90.0, 80.0]);
        let surface = vec![true; r.len()];
        let shadow: Vec<bool> = (0..r.len()).map(|i| i.is_multiple_of(3)).collect();
        let e = measure(&input(&r, &r, &surface, &shadow));
        assert_eq!(e.pixels, r.len());
        assert_eq!(e.shadow_pixels, r.len().div_ceil(3));
        assert!(e.rmse == 0.0 && e.rel_rmse == 0.0, "{e:?}");
        assert_eq!(e.shadow_bias, Some(0.0));
        assert!((e.energy_ratio.expect("lit reference") - 1.0).abs() < 1e-6);
        assert!(e.flip_mean < 1e-6);
        assert!(e.flip_p95 < 1e-6);
    }

    /// ADR-2951: a tenth of the surface far off reads in the p95, above the mean.
    #[test]
    fn the_flip_p95_sees_the_worst_twentieth() {
        let r = plane(|_| [40.0; 3]);
        // Every 10th pixel far off: 10 % of the surface, above the 95th percentile's rank.
        let t = plane(|i| {
            if i.is_multiple_of(10) {
                [400.0; 3]
            } else {
                [40.0; 3]
            }
        });
        let surface = vec![true; r.len()];
        let e = measure(&input(&r, &t, &surface, &surface));
        let (mean, p95) = masked_flip_stats(&r, &t, &surface, W, H, 8.0);
        assert_eq!((mean, p95), (e.flip_mean, e.flip_p95));
        assert!(e.flip_p95 > e.flip_mean && e.flip_mean > 0.0, "{e:?}");
        // Masked out, the same pixels score nothing.
        let none = vec![false; r.len()];
        assert_eq!(masked_flip_stats(&r, &t, &none, W, H, 8.0), (0.0, 0.0));
    }

    #[test]
    fn a_uniform_gain_reads_as_bias_energy_and_rmse() {
        let r = plane(|_| [50.0, 50.0, 50.0]);
        let t = plane(|_| [60.0, 60.0, 60.0]);
        let surface = vec![true; r.len()];
        let e = measure(&input(&r, &t, &surface, &surface));
        assert!((e.rmse - 10.0).abs() < 1e-4, "{e:?}");
        assert!((e.rel_rmse - 0.2).abs() < 1e-4);
        assert!((e.shadow_bias.expect("shadow") - 0.2).abs() < 1e-4);
        assert!((e.energy_ratio.expect("energy") - 1.2).abs() < 1e-4);
        assert!(e.flip_mean > 0.0);
    }

    #[test]
    fn the_shadow_bias_only_sees_shadow_pixels_and_has_a_sign() {
        let r = plane(|_| [40.0, 40.0, 40.0]);
        let shadow: Vec<bool> = (0..r.len()).map(|i| i < r.len() / 2).collect();
        let t = plane(|i| if i < r.len() / 2 { [30.0; 3] } else { [50.0; 3] });
        let surface = vec![true; r.len()];
        let e = measure(&input(&r, &t, &surface, &shadow));
        assert!((e.shadow_bias.expect("shadow") + 0.25).abs() < 1e-4, "{e:?}");
        // Darker shadow, brighter light: the totals cancel.
        assert!((e.energy_ratio.expect("energy") - 1.0).abs() < 1e-4);
        assert!((e.rel_rmse - 0.25).abs() < 1e-4);
    }

    #[test]
    fn masked_pixels_do_not_count() {
        let r = plane(|_| [10.0; 3]);
        let t = plane(|i| if i == 0 { [1000.0; 3] } else { [10.0; 3] });
        let surface: Vec<bool> = (0..r.len()).map(|i| i != 0).collect();
        let none = vec![false; r.len()];
        let e = measure(&input(&r, &t, &surface, &none));
        assert_eq!(e.pixels, r.len() - 1);
        assert!(e.rmse == 0.0 && e.shadow_bias.is_none());
        assert_eq!(e.shadow_pixels, 0);
    }

    #[test]
    fn a_dark_reference_uses_the_given_scale_as_a_leak_fraction() {
        let r = plane(|_| [0.0; 3]);
        let t = plane(|_| [2.0; 3]);
        let surface = vec![true; r.len()];
        let mut i = input(&r, &t, &surface, &surface);
        let e = measure(&i);
        assert!(e.rel_rmse.is_infinite() && e.energy_ratio.is_none() && e.shadow_bias.is_none());
        i.scale = Some(400.0);
        let e = measure(&i);
        assert!((e.rel_rmse - 0.005).abs() < 1e-6, "{e:?}");
        assert!((e.shadow_bias.expect("scaled") - 0.005).abs() < 1e-6);
        assert!(e.energy_ratio.is_none());
    }

    #[test]
    fn mismatched_lengths_score_nothing() {
        let r = plane(|_| [1.0; 3]);
        let surface = vec![true; r.len()];
        let e = measure(&input(&r, &r[1..], &surface, &surface));
        assert_eq!(e.pixels, 0);
        assert!(e.rel_rmse.is_infinite());
    }

    #[test]
    fn the_surface_mask_drops_sky_borders_and_depth_edges() {
        let depth: Vec<f32> = (0..W * H)
            .map(|i| {
                let (x, y) = (i % W, i / W);
                if y < 3 {
                    0.0
                } else if x < 8 {
                    5.0
                } else {
                    9.0
                }
            })
            .collect();
        let m = surface_mask(&depth, W, H, 0.05);
        let at = |x: u32, y: u32| m[(y * W + x) as usize];
        assert!(!at(4, 1), "sky");
        assert!(!at(4, 3), "next to the sky");
        assert!(at(4, 6), "inside the near plane");
        assert!(!at(7, 6) && !at(8, 6), "depth edge");
        assert!(at(11, 6), "inside the far plane");
        assert!(!at(0, 6) && !at(W - 1, 6) && !at(4, H - 1), "border");
    }

    #[test]
    fn the_shadow_mask_takes_occluded_and_unlit_faces() {
        let surface = vec![true, true, true, true, false];
        let visibility = [1.0, 0.2, 1.0, 1.0, 0.0];
        let direct = [[100.0; 3], [0.0; 3], [1.0; 3], [100.0; 3], [0.0; 3]];
        let m = shadow_mask(&surface, &visibility, &direct, 0.05);
        assert_eq!(m, vec![false, true, true, false, false]);
    }

    #[test]
    fn quantiles_are_nearest_rank() {
        assert_eq!(quantile(&[], 0.5), 0.0);
        assert_eq!(quantile(&[3.0, 1.0, 2.0], 0.5), 2.0);
        assert_eq!(quantile(&[3.0, 1.0, 2.0], 1.0), 3.0);
        assert_eq!(quantile(&[3.0, 1.0, 2.0], 0.0), 1.0);
    }

    fn error(rel_rmse: f32, shadow_bias: Option<f32>, energy_ratio: Option<f32>) -> GiError {
        GiError {
            pixels: 10,
            shadow_pixels: 4,
            scale: 1.0,
            mean_reference: 1.0,
            mean_test: 1.0,
            rmse: rel_rmse,
            rel_rmse,
            shadow_bias,
            energy_ratio,
            flip_mean: 0.1,
            flip_p95: 0.2,
        }
    }

    #[test]
    fn regressions_fail_past_the_tolerance_and_never_on_improvement() {
        let record = GiRecord::from_error("trench", "ssgi", &error(0.30, Some(-0.20), Some(0.80)));
        let tol = GiTolerance::REFERENCE;
        assert!(regressions(&error(0.30, Some(-0.20), Some(0.80)), &record, tol).is_empty());
        // Inside the tolerance: 0.30 · 1.1 + 0.02 = 0.35.
        assert!(regressions(&error(0.349, Some(-0.21), Some(0.79)), &record, tol).is_empty());
        let worse = regressions(&error(0.36, Some(-0.30), Some(0.60)), &record, tol);
        assert_eq!(worse.len(), 3, "{worse:?}");
        assert!(worse[0].contains("relative RMSE") && worse[1].contains("shadow bias"));
        assert!(worse[2].contains("energy ratio"));
        // An energy ratio that overshoots 1 by more than the record undershoots is a regression too.
        assert_eq!(
            regressions(&error(0.30, Some(-0.20), Some(1.25)), &record, tol).len(),
            1
        );
        // Better everywhere: no regression, and the improvement is noted.
        let better = error(0.10, Some(-0.01), Some(0.98));
        assert!(regressions(&better, &record, tol).is_empty());
        assert_eq!(improvements(&better, &record, tol).len(), 3);
        // A metric appearing or disappearing is reported.
        assert_eq!(regressions(&error(0.30, None, None), &record, tol).len(), 2);
        // A non-finite measurement never passes.
        assert!(!regressions(&error(f32::INFINITY, Some(-0.2), Some(0.8)), &record, tol).is_empty());
    }

    #[test]
    fn tolerance_decisions_match_absolute_distance_oracle_at_boundaries() {
        fn oracle(recorded: f32, measured_distance: f32, tolerance: GiTolerance) -> bool {
            f64::from(measured_distance)
                <= f64::from(recorded).abs() * (1.0 + f64::from(tolerance.relative))
                    + f64::from(tolerance.absolute)
        }

        let cases: [(f32, GiTolerance, f32); 4] = [
            (
                0.5,
                GiTolerance {
                    relative: 0.0,
                    absolute: 0.125,
                },
                0.625,
            ),
            (
                0.5,
                GiTolerance {
                    relative: 0.25,
                    absolute: 0.0,
                },
                0.625,
            ),
            (
                0.5,
                GiTolerance {
                    relative: 0.25,
                    absolute: 0.125,
                },
                0.75,
            ),
            (
                0.0,
                GiTolerance {
                    relative: 0.5,
                    absolute: 0.125,
                },
                0.125,
            ),
        ];
        for (recorded, tolerance, boundary) in cases {
            let below = f32::from_bits(boundary.to_bits() - 1);
            let above = f32::from_bits(boundary.to_bits() + 1);
            assert!(oracle(recorded, boundary, tolerance));
            assert!(!oracle(recorded, above, tolerance));
            for bias in [-1.0f32, 1.0] {
                let record =
                    GiRecord::from_error("room", "probes", &error(0.0, Some(bias * recorded), Some(1.0)));
                for distance in [below, boundary, above] {
                    let measured = error(0.0, Some(-bias * distance), Some(1.0));
                    let actual = regressions(&measured, &record, tolerance);
                    assert_eq!(
                        actual.iter().filter(|r| r.contains("shadow bias")).count(),
                        usize::from(!oracle(recorded, distance, tolerance))
                    );
                    assert_eq!(
                        actual.len(),
                        actual.iter().filter(|r| r.contains("shadow bias")).count()
                    );
                }
                let energy_record = GiRecord::from_error(
                    "room",
                    "probes",
                    &error(0.0, Some(0.0), Some(1.0 + bias * recorded)),
                );
                let energy_boundary = 1.0 - bias * boundary;
                let (toward_one, away_from_one) = if energy_boundary < 1.0 {
                    (
                        f32::from_bits(energy_boundary.to_bits() + 1),
                        f32::from_bits(energy_boundary.to_bits() - 1),
                    )
                } else {
                    (
                        f32::from_bits(energy_boundary.to_bits() - 1),
                        f32::from_bits(energy_boundary.to_bits() + 1),
                    )
                };
                for ratio in [toward_one, energy_boundary, away_from_one] {
                    let measured = error(0.0, Some(0.0), Some(ratio));
                    let actual = regressions(&measured, &energy_record, tolerance);
                    assert_eq!(
                        actual.iter().filter(|r| r.contains("energy ratio")).count(),
                        usize::from(!oracle(recorded, (ratio - 1.0).abs(), tolerance)),
                        "recorded={recorded}, ratio={ratio}, tolerance={tolerance:?}, reasons={actual:?}"
                    );
                    assert_eq!(
                        actual.len(),
                        actual.iter().filter(|r| r.contains("energy ratio")).count()
                    );
                }
            }
        }
    }

    #[test]
    fn improvements_include_energy_distance_from_one_and_remain_advisory() {
        let record = GiRecord::from_error("room", "probes", &error(0.5, Some(-0.4), Some(1.4)));
        let measured = error(0.1, Some(0.01), Some(1.01));
        let notes = improvements(
            &measured,
            &record,
            GiTolerance {
                relative: 0.1,
                absolute: 0.01,
            },
        );
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes.iter().any(|note| note.contains("energy ratio")));
        assert!(
            regressions(
                &measured,
                &record,
                GiTolerance {
                    relative: 0.1,
                    absolute: 0.01
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn relative_rmse_uses_the_same_inclusive_boundary_and_only_its_own_reason() {
        fn oracle(recorded: f32, measured: f32, tolerance: GiTolerance) -> bool {
            f64::from(measured)
                <= f64::from(recorded).abs() * (1.0 + f64::from(tolerance.relative))
                    + f64::from(tolerance.absolute)
        }

        let cases: [(f32, GiTolerance, f32); 4] = [
            (
                0.5,
                GiTolerance {
                    relative: 0.0,
                    absolute: 0.125,
                },
                0.625,
            ),
            (
                0.5,
                GiTolerance {
                    relative: 0.25,
                    absolute: 0.0,
                },
                0.625,
            ),
            (
                0.5,
                GiTolerance {
                    relative: 0.25,
                    absolute: 0.125,
                },
                0.75,
            ),
            (
                0.0,
                GiTolerance {
                    relative: 0.5,
                    absolute: 0.125,
                },
                0.125,
            ),
        ];
        for (recorded, tolerance, boundary) in cases {
            let record = GiRecord::from_error("room", "probes", &error(recorded, Some(0.0), Some(1.0)));
            let below = f32::from_bits(boundary.to_bits() - 1);
            let above = f32::from_bits(boundary.to_bits() + 1);
            for measured in [below, boundary, above] {
                let actual = regressions(&error(measured, Some(0.0), Some(1.0)), &record, tolerance);
                assert_eq!(
                    actual
                        .iter()
                        .filter(|reason| reason.contains("relative RMSE"))
                        .count(),
                    usize::from(!oracle(recorded, measured, tolerance)),
                    "recorded={recorded}, measured={measured}, tolerance={tolerance:?}, reasons={actual:?}"
                );
                assert_eq!(
                    actual.len(),
                    actual
                        .iter()
                        .filter(|reason| reason.contains("relative RMSE"))
                        .count()
                );
            }
        }
    }

    #[test]
    fn missing_shadow_and_dark_reference_transitions_name_only_the_changed_metrics() {
        let tolerance = GiTolerance {
            relative: 0.1,
            absolute: 0.01,
        };
        let measured = error(0.2, None, None);
        let record = GiRecord::from_error("room", "probes", &error(0.2, Some(0.0), None));
        let reasons = regressions(&measured, &record, tolerance);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("shadow bias") && reasons[0].contains("no shadow pixels"));

        let record = GiRecord::from_error("room", "probes", &error(0.2, None, Some(1.0)));
        let reasons = regressions(&measured, &record, tolerance);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(reasons[0].contains("energy ratio") && reasons[0].contains("reference is dark"));

        let measured = error(0.2, Some(0.0), Some(1.0));
        let record = GiRecord::from_error("room", "probes", &error(0.2, None, None));
        let reasons = regressions(&measured, &record, tolerance);
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(reasons.iter().any(|reason| reason.contains("shadow bias")));
        assert!(reasons.iter().any(|reason| reason.contains("energy ratio")));
    }

    #[test]
    fn finite_record_roundtrip_preserves_comparison_precision() {
        let value = f32::from_bits(0x3e_fc_3f_35);
        let record = GiRecord::from_error("room", "probes", &error(value, Some(-value), Some(1.0 + value)));
        let formatted = format_records(std::slice::from_ref(&record), "test");
        let parsed = parse_records(&formatted).expect("ordinary finite record parses");
        assert_eq!(parsed, [record]);
    }

    #[test]
    fn the_other_adapter_tolerance_is_looser() {
        let record = GiRecord::from_error("room", "probes", &error(0.40, None, Some(1.0)));
        let m = error(0.55, None, Some(1.0));
        assert!(!regressions(&m, &record, GiTolerance::REFERENCE).is_empty());
        assert!(regressions(&m, &record, GiTolerance::OTHER_ADAPTER).is_empty());
    }

    #[test]
    fn records_round_trip_through_the_text_format() {
        let records = vec![
            GiRecord::from_error("trench", "ambient", &error(0.41234, Some(-0.25), Some(0.8))),
            GiRecord::from_error("sealed_box", "probes", &error(0.004, Some(0.003), None)),
        ];
        let text = format_records(&records, "llvmpipe (LLVM 23)\nsecond line");
        assert_eq!(text.lines().filter(|l| l.starts_with('#')).count(), 3);
        let back = parse_records(&text).expect("parse");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].scene, "trench");
        assert!((back[0].rel_rmse - 0.41234).abs() < 1e-5);
        assert_eq!(back[1].energy_ratio, None);
        assert!((back[1].shadow_bias.expect("bias") - 0.003).abs() < 1e-6);
    }

    #[test]
    fn malformed_records_name_their_line() {
        let e = parse_records("# header\ntrench ambient 0.1 -\n").expect_err("4 fields");
        assert!(e.starts_with("line 2:"), "{e}");
        let e = parse_records("Trench ambient 0.1 - -\n").expect_err("uppercase");
        assert!(e.contains("lowercase"), "{e}");
        let e = parse_records("trench ambient - - -\n").expect_err("rel_rmse");
        assert!(e.contains("required"), "{e}");
        let e = parse_records("trench ambient 0.1 nan -\n").expect_err("nan");
        assert!(e.contains("shadow_bias"), "{e}");
    }

    #[test]
    fn records_are_written_atomically_and_read_back() {
        let dir = crate::unique_temp_dir("gi-records");
        let path = dir.join("nested").join("scene.txt");
        let records = vec![GiRecord::from_error(
            "scene",
            "baked",
            &error(0.2, Some(0.1), Some(1.1)),
        )];
        write_records(&path, &records, "test adapter").expect("write");
        assert_eq!(read_records(&path).expect("read"), records);
        assert!(read_records(&dir.join("missing.txt")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_table_has_a_row_per_technique() {
        let rows = vec![
            GiRow {
                scene: "trench".into(),
                technique: "ambient".into(),
                error: error(0.5, Some(-0.3), Some(0.7)),
            },
            GiRow {
                scene: "trench".into(),
                technique: "baked".into(),
                error: error(0.1, None, None),
            },
        ];
        let t = render_table(&rows);
        assert_eq!(t.lines().count(), 4);
        assert!(t.contains("| trench | ambient |") && t.contains("-0.300") && t.contains("| – |"));
    }

    #[test]
    fn planes_add_and_subtract() {
        let a = [[1.0, 2.0, 3.0]];
        let b = [[0.5, 0.5, 4.0]];
        assert_eq!(add_planes(&a, &b), vec![[1.5, 2.5, 7.0]]);
        assert_eq!(sub_planes(&a, &b), vec![[0.5, 1.5, -1.0]]);
    }
}
