// SPDX-License-Identifier: MPL-2.0
//! Runs a quality preset on a [`PathTracer`]: the biased light-transport limits, adaptive sampling
//! by bands, firefly suppression and the built-in (or OIDN) preview denoiser (ADR-2433).

use crate::PtError;
use crate::denoise_atrous::{AtrousSettings, filter_gpu, suppress_fireflies, variance_from_image};
use crate::output::PtImage;
use crate::presets::{BIASED_MARK, DenoiserKind, PresetSettings, PtPreset};
use crate::scene::PtScene;
use crate::tracer::{PathTracer, PtSession, PtSettings};

const WORKGROUP: u32 = 64;

impl PtPreset {
    /// The settings table with the cost knobs taken from `base` for [`Self::Reference`] (so a
    /// reference trace uses exactly the caller's `spp`, bounces and Russian-roulette start); the
    /// other presets return [`Self::settings`].
    #[must_use]
    pub fn settings_for(self, base: &PtSettings) -> PresetSettings {
        let mut s = self.settings();
        if self == Self::Reference {
            s.spp = base.spp;
            s.max_bounces = base.max_bounces;
            s.rr_start = base.rr_start;
        }
        s
    }
}

/// How many samples one horizontal band of the image took.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct TileReport {
    /// First row of the band.
    pub first_row: u32,
    /// Rows in the band.
    pub rows: u32,
    /// Mean samples per pixel the band took.
    pub spp: f32,
    /// The band's relative standard error at the end (0 for a band that never varied).
    pub error: f32,
}

/// What a preset run did.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct PresetReport {
    /// The preset's name.
    pub preset: String,
    /// Samples per pixel asked for (the cap with adaptive sampling).
    pub spp_cap: u32,
    /// Pixel samples actually traced, summed over the image.
    pub pixel_samples: u64,
    /// Pixel samples a uniform trace at the cap would take.
    pub pixel_samples_uniform: u64,
    /// Per-band sample counts (empty without adaptive sampling).
    pub tiles: Vec<TileReport>,
    /// Denoiser applied (`none`, `builtin`, `oidn`).
    pub denoiser: String,
    /// Firefly threshold applied (0 = off).
    pub firefly_threshold: f32,
    /// True when the settings are the base tracer's: the image is bit-identical to a trace made
    /// without presets.
    pub base: bool,
}

/// A preset run's output.
#[derive(Clone, Debug)]
pub struct PresetResult {
    /// The image to use: denoised and firefly-filtered when the preset asks for it (then it
    /// carries [`crate::output::DENOISER_KEY`] when denoised).
    pub image: PtImage,
    /// The raw accumulation, when `image` was post-processed (None when they are the same).
    pub raw: Option<PtImage>,
    /// What was done.
    pub report: PresetReport,
}

fn pixel_counts(image: &PtImage) -> Vec<f32> {
    if image.spp_map.len() == image.beauty.len() {
        image.spp_map.clone()
    } else {
        vec![image.spp as f32; image.beauty.len()]
    }
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Per-pixel luminance variance of the mean: the mean of the half-buffer estimate (below) and the
/// accumulator's own second-moment estimate. The half-buffer part is from a half-way snapshot `a` and the full image:
/// the second part is `b = (n·full − h·a) / (n − h)`, and `Var(mean) = (a − b)² · h(n−h) / n²`
/// (`(a − b)² / 4` for equal halves). Pixels with no second part use the image's own variance.
fn variance_estimate(full: &PtImage, snapshot: Option<&PtImage>) -> Vec<f32> {
    let moment = variance_from_image(full);
    let Some(a) = snapshot else {
        return moment;
    };
    let (nf, na) = (pixel_counts(full), pixel_counts(a));
    (0..moment.len())
        .map(|i| {
            let (n, h) = (nf[i], na[i]);
            if h < 1.0 || n - h < 1.0 {
                return moment[i];
            }
            let lf = luminance(full.beauty[i]);
            let la = luminance(a.beauty[i]);
            let lb = (n * lf - h * la) / (n - h);
            let v = (la - lb) * (la - lb) * h * (n - h) / (n * n);
            // The halves are one degree of freedom; the accumulator's second moment adds the
            // other samples' evidence. Both estimate the same variance of the mean.
            if v.is_finite() {
                0.5 * (v + moment[i])
            } else {
                moment[i]
            }
        })
        .collect()
}

/// Relative standard error of one band from a variance plane already converted for the frame.
fn band_error(image: &PtImage, var: &[f32], row0: u32, rows: u32, floor: f32) -> f32 {
    let w = image.width as usize;
    let (a, b) = (
        row0 as usize * w,
        ((row0 + rows) as usize * w).min(image.beauty.len()),
    );
    let n = (b - a).max(1) as f32;
    let mean: f32 = image.beauty[a..b].iter().map(|c| luminance(*c)).sum::<f32>() / n;
    let end = b.min(var.len());
    let v: f32 = var[a.min(end)..end].iter().sum::<f32>() / n;
    v.max(0.0).sqrt() / mean.max(floor).max(f32::MIN_POSITIVE)
}

/// One relative error per horizontal band of `tile_rows` (the last band may be shorter).
/// The variance plane is converted once for the whole frame. `active` skips a band when it is
/// `Some` and that entry is not true; skipped bands stay 0.
fn band_errors_of(image: &PtImage, tile_rows: u32, floor: f32, active: Option<&[bool]>) -> Vec<f32> {
    let h = image.height;
    let bands = h.div_ceil(tile_rows.max(1)) as usize;
    let var = variance_from_image(image);
    let mut errors = vec![0.0f32; bands];
    for (band, error) in errors.iter_mut().enumerate() {
        if let Some(on) = active
            && on.get(band).copied() != Some(true)
        {
            continue;
        }
        let r0 = band as u32 * tile_rows;
        let rows = tile_rows.min(h - r0);
        *error = band_error(image, &var, r0, rows, floor);
    }
    errors
}

/// Workgroup ranges `(first, count)` covering the active bands, adjacent ones merged.
fn active_ranges(width: u32, height: u32, tile_rows: u32, active: &[bool]) -> Vec<(u32, u32)> {
    let total = (width * height).div_ceil(WORKGROUP);
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for (band, on) in active.iter().enumerate() {
        if !on {
            continue;
        }
        let r0 = band as u32 * tile_rows;
        let r1 = ((band as u32 + 1) * tile_rows).min(height);
        let first = r0 * width / WORKGROUP;
        let end = (r1 * width).div_ceil(WORKGROUP).min(total);
        match ranges.last_mut() {
            Some(last) if last.0 + last.1 >= first => last.1 = end.saturating_sub(last.0).max(last.1),
            _ => ranges.push((first, end.saturating_sub(first))),
        }
    }
    ranges
}

/// Traces `scene` with `preset` on top of `base` (seed, jitter and submit size come from `base`;
/// for [`PtPreset::Reference`] build the table with [`PtPreset::settings_for`] so the cost knobs
/// do too).
///
/// With a base-equal table (nothing biased, no adaptive sampling, no denoiser, no firefly filter)
/// this is `PathTracer::render` exactly: the same dispatches, the same bits.
///
/// # Errors
///
/// Invalid settings (zero or huge spp, NaN thresholds, ...), a scene that cannot be traced, a GPU
/// failure, or the OIDN denoiser asked for in a build without the `denoise` feature.
pub fn render_preset(
    tracer: &PathTracer,
    scene: &PtScene,
    base: &PtSettings,
    preset: &PresetSettings,
) -> Result<PresetResult, PtError> {
    preset.validate()?;
    let settings = preset.trace_settings(base);
    let mut session = tracer.begin_with_quality(scene, &settings, &preset.quality)?;
    run_preset(&mut session, preset, &mut |_| true)
}

/// Runs `preset` on a session begun with `preset.quality` (see
/// [`PathTracer::begin_cached_with_quality`]), calling `progress(samples_done)` after every
/// submission; returning `false` cancels (the image so far is returned). The session must be
/// fresh: no samples traced yet, and begun with the preset's `spp`, bounces and quality.
///
/// # Errors
///
/// See [`render_preset`].
pub fn run_preset(
    session: &mut PtSession<'_>,
    preset: &PresetSettings,
    progress: &mut dyn FnMut(u32) -> bool,
) -> Result<PresetResult, PtError> {
    let _span = tracing::info_span!(
        "regia_pt::render_preset",
        preset = preset.preset.name(),
        spp = preset.spp
    )
    .entered();
    preset.validate()?;
    if session.samples_done() != 0 {
        return Err(crate::presets::bad("the session already has samples"));
    }
    let tracer = session.tracer();
    let (w, h) = session.size();
    let uniform = u64::from(w) * u64::from(h) * u64::from(preset.spp);
    let needs_halves = preset.denoiser == DenoiserKind::BuiltIn && preset.spp >= 2;
    let half = preset.spp / 2;
    let mut snapshot: Option<PtImage> = None;
    let mut tiles = Vec::new();
    let image;
    if preset.adaptive.enabled {
        let ad = &preset.adaptive;
        let bands = h.div_ceil(ad.tile_rows) as usize;
        let mut active = vec![true; bands];
        let mut errors = vec![0.0f32; bands];
        let mut done = 0u32;
        let mut live = true;
        while live && done < preset.spp && active.iter().any(|&a| a) {
            let mut step = ad.round_spp.min(preset.spp - done);
            if needs_halves && snapshot.is_none() && done < half && done + step > half {
                step = half - done;
            }
            let ranges = active_ranges(w, h, ad.tile_rows, &active);
            session.run_ranges(step, &ranges, |n| {
                live = progress(n);
                live
            })?;
            done += step;
            let want_snap = needs_halves && snapshot.is_none() && done >= half;
            let want_stop = done >= ad.min_spp && done < preset.spp;
            if want_snap || want_stop {
                // Beauty and the second moment are enough for the snapshot and the band test.
                // One readback serves both. The frame returned below is still a full decode.
                let now = session.image_stopping()?;
                if want_stop {
                    let mean_lum: f32 = now.beauty.iter().map(|c| luminance(*c)).sum::<f32>()
                        / now.beauty.len().max(1) as f32;
                    let errs = band_errors_of(&now, ad.tile_rows, 0.01 * mean_lum, Some(&active));
                    for (band, on) in active.iter_mut().enumerate() {
                        if !*on {
                            continue;
                        }
                        let err = errs[band];
                        errors[band] = err;
                        if err < ad.threshold {
                            *on = false;
                        }
                    }
                }
                if want_snap {
                    snapshot = Some(now);
                }
            }
        }
        image = session.image()?;
        let counts = pixel_counts(&image);
        let report_errs = band_errors_of(&image, ad.tile_rows, 1e-4, None);
        for band in 0..bands {
            let r0 = band as u32 * ad.tile_rows;
            let rows = ad.tile_rows.min(h - r0);
            let (a, b) = (r0 as usize * w as usize, (r0 + rows) as usize * w as usize);
            let spp = counts[a..b].iter().sum::<f32>() / (b - a).max(1) as f32;
            let error = report_errs[band];
            tiles.push(TileReport {
                first_row: r0,
                rows,
                spp,
                error: if active[band] {
                    error
                } else {
                    errors[band].min(error)
                },
            });
        }
    } else if needs_halves {
        let mut live = true;
        session.run_with(half, |n| {
            live = progress(n);
            live
        })?;
        snapshot = Some(session.image_stopping()?);
        if live {
            session.run_with(preset.spp - half, &mut *progress)?;
        }
        image = session.image()?;
    } else {
        session.run_with(preset.spp, &mut *progress)?;
        image = session.image()?;
    }
    let pixel_samples: u64 = pixel_counts(&image).iter().map(|&c| c as u64).sum();

    let post = preset.firefly_threshold > 0.0 || preset.denoiser != DenoiserKind::None;
    let (mut final_image, raw) = if post {
        let mut out = image.clone();
        if preset.firefly_threshold > 0.0 {
            suppress_fireflies(&mut out.beauty, out.width, out.height, preset.firefly_threshold)?;
        }
        match preset.denoiser {
            DenoiserKind::None => {}
            DenoiserKind::BuiltIn => {
                let variance = variance_estimate(&image, snapshot.as_ref());
                out = filter_gpu(tracer.device(), &out, &variance, &AtrousSettings::default())?;
            }
            DenoiserKind::Oidn => {
                #[cfg(feature = "denoise")]
                {
                    out = crate::denoise::denoise(&out, &crate::denoise::DenoiseOptions::default())?;
                }
                #[cfg(not(feature = "denoise"))]
                {
                    return Err(crate::presets::bad(
                        "the oidn denoiser needs the `denoise` feature (this build lacks it)",
                    ));
                }
            }
        }
        (out, Some(image))
    } else {
        (image, None)
    };
    // Clamps, the caustics cut and the firefly filter remove energy: such an image is a preview,
    // so it carries the denoiser mark too and `compare` refuses it as a reference.
    if final_image.metadata.denoiser.is_empty()
        && (preset.firefly_threshold > 0.0 || !preset.quality.is_base())
    {
        final_image.metadata.denoiser = format!("{BIASED_MARK} {}", preset.preset.name());
    }
    Ok(PresetResult {
        report: PresetReport {
            preset: preset.preset.name().to_string(),
            spp_cap: preset.spp,
            pixel_samples,
            pixel_samples_uniform: uniform,
            tiles,
            denoiser: preset.denoiser.name().to_string(),
            firefly_threshold: preset.firefly_threshold,
            base: preset.is_base(),
        },
        image: final_image,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_become_merged_workgroup_ranges() {
        // 64 px wide: one workgroup per row.
        let all = active_ranges(64, 8, 2, &[true, true, true, true]);
        assert_eq!(all, vec![(0, 8)]);
        let gap = active_ranges(64, 8, 2, &[true, false, false, true]);
        assert_eq!(gap, vec![(0, 2), (6, 2)]);
        assert!(active_ranges(64, 8, 2, &[false; 4]).is_empty());
        // A width that is not a multiple of 64 shares boundary groups but never overruns.
        let odd = active_ranges(40, 10, 3, &[true, true, true, true]);
        let total = (40u32 * 10).div_ceil(64);
        assert_eq!(odd.iter().map(|r| r.0 + r.1).max(), Some(total));
    }

    #[test]
    fn variance_estimate_uses_halves_when_it_can() {
        let mk = |v: f32, spp: u32| PtImage {
            width: 1,
            height: 1,
            beauty: vec![[v; 3]],
            albedo: vec![[0.5; 3]],
            normal: vec![[0.0, 0.0, 1.0]],
            depth: vec![1.0],
            direct: vec![[0.0; 3]],
            indirect_diffuse: vec![[0.0; 3]],
            indirect_specular: vec![[0.0; 3]],
            emissive: vec![[0.0; 3]],
            volumetric: vec![[0.0; 3]],
            sky: vec![[0.0; 3]],
            shadow: vec![1.0],
            roughness: vec![0.5],
            variance: vec![[7.0; 3]],
            spp,
            spp_map: Vec::new(),
            metadata: crate::PtMetadata::default(),
        };
        // Halves of 4 samples with means 2 and 4: full mean 3, variance of the mean (2)²/4 = 1.
        // The preset feeds the filter the average of that term and the accumulator moment
        // (`book/agents/pt-presets.md`).
        let moment = variance_from_image(&mk(3.0, 8))[0];
        let v = variance_estimate(&mk(3.0, 8), Some(&mk(2.0, 4)));
        assert!(
            (v[0] - 0.5 * (1.0 + moment)).abs() < 1e-5,
            "{v:?} (halves 1, moment {moment})"
        );
        // No second half: the image's own variance (luminance weights squared).
        let own = variance_estimate(&mk(3.0, 4), Some(&mk(2.0, 4)));
        assert!((own[0] - variance_from_image(&mk(3.0, 4))[0]).abs() < 1e-6);
    }

    #[test]
    fn band_errors_convert_the_variance_plane_once() {
        use std::cell::Cell;
        // Four bands, two rows each. Band b's variance is `(b + 1)` on every channel, beauty is 1,
        // so the relative error is `sqrt(luminance-weight · variance)` and the bands must not share
        // one plane scan that mixes them.
        let bands = 4usize;
        let rows = 2usize;
        let width = 2usize;
        let n = bands * rows * width;
        let mut beauty = vec![[1.0f32; 3]; n];
        let mut variance = vec![[0.0f32; 3]; n];
        for b in 0..bands {
            let v = (b + 1) as f32;
            let start = b * rows * width;
            let end = start + rows * width;
            for pixel in &mut variance[start..end] {
                *pixel = [v; 3];
            }
        }
        let _ = &mut beauty;
        let image = PtImage {
            width: width as u32,
            height: (bands * rows) as u32,
            beauty,
            albedo: vec![[0.5; 3]; n],
            normal: vec![[0.0, 0.0, 1.0]; n],
            depth: vec![1.0; n],
            direct: vec![[0.0; 3]; n],
            indirect_diffuse: vec![[0.0; 3]; n],
            indirect_specular: vec![[0.0; 3]; n],
            emissive: vec![[0.0; 3]; n],
            volumetric: vec![[0.0; 3]; n],
            sky: vec![[0.0; 3]; n],
            shadow: vec![1.0; n],
            roughness: vec![0.5; n],
            variance,
            spp: 8,
            spp_map: Vec::new(),
            metadata: crate::PtMetadata::default(),
        };
        let weight = 0.2126f32 * 0.2126 + 0.7152 * 0.7152 + 0.0722 * 0.0722;
        let before = crate::denoise_atrous::VARIANCE_CONVERSIONS.with(Cell::get);
        let errors = band_errors_of(&image, rows as u32, 1e-4, None);
        let calls = crate::denoise_atrous::VARIANCE_CONVERSIONS.with(Cell::get) - before;
        assert_eq!(errors.len(), bands);
        for (b, err) in errors.iter().enumerate() {
            let expect = (weight * (b + 1) as f32).sqrt();
            assert!((err - expect).abs() < 1e-5, "band {b}: {err} vs {expect}");
        }
        assert_eq!(
            calls, 1,
            "variance plane converted {calls} times for {bands} bands"
        );
    }
}
