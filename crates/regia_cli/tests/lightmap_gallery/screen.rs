// SPDX-License-Identifier: MPL-2.0
//! Screen-space checks of the gallery plate against the path tracer at the same frame (ADR-4108).
//! Pixels are placed in the world from the tracer's first-hit depth and normal, sorted into the
//! gallery's large flat regions, and binned into square cells on each region's plane (so a cell is
//! the same patch of wall in both images, whatever its distance), and three figures are measured:
//!
//! - **Mean colour** of each region, the frame's against the tracer's: the luminance ratio and the
//!   chromaticity error (`R/G` and `B/G` of the frame over the tracer's, minus one).
//! - **Flatness**: the relative RMS of the cells' luminance after a least-squares quadric over the
//!   region (the light's slow change across a wall), for the frame and for the tracer, beside the
//!   tracer's own noise at that size (one seed against the two-seed mean). Mottled light (patches a
//!   few cells across) raises the frame's figure above the tracer's; the ratio of the cells (frame
//!   over tracer) is reported too.
//! - **Seam step** across a straight border: the mean of `|left − right| / mean` over strips half a
//!   metre wide on each side, per half-metre run of the border, for the frame and the tracer. The
//!   tracer has no charts, so its figure is the light's own change across one metre plus its noise.

use regia_core::glam::Vec3;
use serde_json::{Value, json};

/// Rec. 709 luminance.
fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// World position and normal of every pixel's first hit, from the tracer's view depth (the hit's
/// distance along the camera's forward axis) and first-hit normal; the camera is the tracer's
/// (`PtCamera::basis`, a vertical field of view of 1 radian, no jitter, pixel centres).
pub fn first_hits(
    depth: &[f32],
    normal: &[[f32; 3]],
    eye: [f32; 3],
    target: [f32; 3],
    width: u32,
    height: u32,
) -> Vec<Option<(Vec3, Vec3)>> {
    let eye = Vec3::from_array(eye);
    let fwd = (Vec3::from_array(target) - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd).normalize();
    let th = (0.5f32).tan();
    let aspect = width as f32 / height as f32;
    (0..(width * height) as usize)
        .map(|i| {
            let d = depth[i];
            if !(d > 0.0 && d.is_finite()) {
                return None;
            }
            let (x, y) = ((i % width as usize) as f32, (i / width as usize) as f32);
            let sx = (x + 0.5) / width as f32 * 2.0 - 1.0;
            let sy = 1.0 - (y + 0.5) / height as f32 * 2.0;
            let ray = fwd + right * (sx * th * aspect) + up * (sy * th);
            Some((eye + ray * d, Vec3::from_array(normal[i]).normalize_or_zero()))
        })
        .collect()
}

/// A flat region of the gallery: the plane `p[axis] = value` facing `n`, its rectangle in the two
/// other axes `(a, b)`, and the cell size.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// Name.
    pub name: &'static str,
    /// The plane's axis (0 x, 1 y, 2 z).
    pub axis: usize,
    /// The plane's coordinate.
    pub value: f32,
    /// Outward normal.
    pub n: Vec3,
    /// The in-plane axes.
    pub ab: (usize, usize),
    /// `[a0, a1, b0, b1]`.
    pub rect: [f32; 4],
}

impl Region {
    /// The in-plane coordinates of `hit` when it is on the region.
    pub fn coords(&self, hit: Option<(Vec3, Vec3)>) -> Option<(f32, f32)> {
        let (p, n) = hit?;
        if (p[self.axis] - self.value).abs() > 0.03 || n.dot(self.n) < 0.95 {
            return None;
        }
        let (a, b) = (p[self.ab.0], p[self.ab.1]);
        let [a0, a1, b0, b1] = self.rect;
        (a >= a0 && a <= a1 && b >= b0 && b <= b1).then_some((a, b))
    }
}

/// Mean colour of the pixels in `mask`.
pub fn mean_rgb(img: &[[f32; 3]], mask: &[bool]) -> [f32; 3] {
    let (mut s, mut n) = ([0.0f64; 3], 0u32);
    for (c, _) in img.iter().zip(mask).filter(|(_, m)| **m) {
        for k in 0..3 {
            s[k] += f64::from(c[k]);
        }
        n += 1;
    }
    s.map(|v| (v / f64::from(n.max(1))) as f32)
}

/// Luminance ratio and chromaticity error of `test` against `reference` (module docs).
pub fn colour_error(test: [f32; 3], reference: [f32; 3]) -> Value {
    let ratio = |t: f32, r: f32| if r > 0.0 { t / r } else { f32::NAN };
    let rg = ratio(test[0] / test[1].max(1e-9), reference[0] / reference[1].max(1e-9)) - 1.0;
    let bg = ratio(test[2] / test[1].max(1e-9), reference[2] / reference[1].max(1e-9)) - 1.0;
    json!({
        "test": test,
        "reference": reference,
        "luminance_ratio": ratio(lum(test), lum(reference)),
        "rg_error": rg,
        "bg_error": bg,
        "chroma_error": rg.abs().max(bg.abs()),
    })
}

/// Cell means of the luminance of `planes` over the region's pixels: per cell `(a, b, means)`.
fn cells(
    region: &Region,
    hits: &[Option<(Vec3, Vec3)>],
    planes: &[&[[f32; 3]]],
    cell: f32,
    min_pixels: u32,
) -> Vec<(f32, f32, Vec<f32>)> {
    let mut map: std::collections::BTreeMap<(i32, i32), (u32, Vec<f64>)> = std::collections::BTreeMap::new();
    for (i, h) in hits.iter().enumerate() {
        let Some((a, b)) = region.coords(*h) else {
            continue;
        };
        let key = ((a / cell).floor() as i32, (b / cell).floor() as i32);
        let e = map.entry(key).or_insert_with(|| (0, vec![0.0; planes.len()]));
        e.0 += 1;
        for (s, p) in e.1.iter_mut().zip(planes) {
            *s += f64::from(lum(p[i]));
        }
    }
    map.into_iter()
        .filter(|(_, (n, _))| *n >= min_pixels)
        .map(|((ka, kb), (n, s))| {
            (
                (ka as f32 + 0.5) * cell,
                (kb as f32 + 0.5) * cell,
                s.iter().map(|v| (*v / f64::from(n)) as f32).collect(),
            )
        })
        .collect()
}

/// Relative RMS of `values` at `(a, b)` after a least-squares quadric in `(a, b)`.
fn quadric_residual(points: &[(f32, f32, f32)]) -> Option<f32> {
    if points.len() < 12 {
        return None;
    }
    let basis = |a: f64, b: f64| [1.0, a, b, a * a, a * b, b * b];
    let mut m = [[0.0f64; 6]; 6];
    let mut r = [0.0f64; 6];
    for &(a, b, v) in points {
        let x = basis(f64::from(a), f64::from(b));
        for ((row, ri), xi) in m.iter_mut().zip(r.iter_mut()).zip(x) {
            for (mij, xj) in row.iter_mut().zip(x) {
                *mij += xi * xj;
            }
            *ri += xi * f64::from(v);
        }
    }
    // Gaussian elimination with partial pivoting.
    for c in 0..6 {
        let p = (c..6).max_by(|&i, &j| m[i][c].abs().total_cmp(&m[j][c].abs()))?;
        if m[p][c].abs() < 1e-12 {
            return None;
        }
        m.swap(c, p);
        r.swap(c, p);
        let (pivot, pr) = (m[c], r[c]);
        for (row, ri) in m.iter_mut().zip(r.iter_mut()).skip(c + 1) {
            let f = row[c] / pivot[c];
            for (x, pv) in row.iter_mut().zip(pivot).skip(c) {
                *x -= f * pv;
            }
            *ri -= f * pr;
        }
    }
    let mut coef = [0.0f64; 6];
    for c in (0..6).rev() {
        let s: f64 = (c + 1..6).map(|j| m[c][j] * coef[j]).sum();
        coef[c] = (r[c] - s) / m[c][c];
    }
    let mean = points.iter().map(|p| f64::from(p.2)).sum::<f64>() / points.len() as f64;
    if mean <= 0.0 {
        return None;
    }
    let sq: f64 = points
        .iter()
        .map(|&(a, b, v)| {
            let x = basis(f64::from(a), f64::from(b));
            let fit: f64 = x.iter().zip(coef).map(|(x, c)| x * c).sum();
            (f64::from(v) - fit).powi(2)
        })
        .sum();
    Some(((sq / points.len() as f64).sqrt() / mean) as f32)
}

/// Relative standard deviation of `values`.
fn rel_std(values: &[f32]) -> Option<f32> {
    if values.len() < 2 {
        return None;
    }
    let n = values.len() as f64;
    let m = values.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
    let var = values.iter().map(|v| (f64::from(*v) - m).powi(2)).sum::<f64>() / n;
    (m > 0.0).then(|| (var.sqrt() / m) as f32)
}

/// What one region measures in one view.
pub struct RegionScores {
    /// Cells with enough pixels.
    pub cells: usize,
    /// The JSON row: `pixels`, `cells`, `cell_m`, `flatness { frame, reference, reference_noise,
    /// ratio_spread }` and the colour errors.
    pub json: Value,
}

/// The images a region is scored on: the indirect light of the frame and of the tracer (and one
/// seed of the tracer's), and the whole frames.
pub struct Planes<'a> {
    /// First-hit positions and normals ([`first_hits`]).
    pub hits: &'a [Option<(Vec3, Vec3)>],
    /// The frame's indirect light.
    pub frame_indirect: &'a [[f32; 3]],
    /// The tracer's indirect light (two seeds).
    pub reference_indirect: &'a [[f32; 3]],
    /// One seed of it.
    pub reference_a: &'a [[f32; 3]],
    /// Its diffuse lobe only (what a lightmap holds; the rest is the tracer's indirect specular).
    pub reference_diffuse: &'a [[f32; 3]],
    /// The whole frame.
    pub frame_beauty: &'a [[f32; 3]],
    /// The tracer's whole frame.
    pub reference_beauty: &'a [[f32; 3]],
}

/// The region's figures (module docs) at cells of `cell` metres.
pub fn score_region(region: &Region, planes: &Planes<'_>, cell: f32) -> RegionScores {
    let Planes {
        hits,
        frame_indirect,
        reference_indirect,
        reference_a,
        reference_diffuse,
        frame_beauty,
        reference_beauty,
    } = *planes;
    let mask: Vec<bool> = hits.iter().map(|h| region.coords(*h).is_some()).collect();
    let pixels = mask.iter().filter(|m| **m).count();
    let cs = cells(
        region,
        hits,
        &[frame_indirect, reference_indirect, reference_a],
        cell,
        6,
    );
    let pick = |k: usize| -> Vec<(f32, f32, f32)> { cs.iter().map(|c| (c.0, c.1, c.2[k])).collect() };
    let flatness_frame = quadric_residual(&pick(0));
    let flatness_reference = quadric_residual(&pick(1));
    let noise: Vec<f32> = cs
        .iter()
        .filter(|c| c.2[1] > 0.0)
        .map(|c| c.2[2] / c.2[1])
        .collect();
    let reference_noise = rel_std(&noise);
    let ratios: Vec<f32> = cs
        .iter()
        .filter(|c| c.2[1] > 0.0)
        .map(|c| c.2[0] / c.2[1])
        .collect();
    let ratio_spread = rel_std(&ratios);
    let json = json!({
        "pixels": pixels,
        "cells": cs.len(),
        "cell_m": cell,
        "flatness": {
            "frame": flatness_frame,
            "reference": flatness_reference,
            "reference_noise": reference_noise,
            "ratio_spread": ratio_spread,
        },
        "indirect_colour": colour_error(mean_rgb(frame_indirect, &mask), mean_rgb(reference_indirect, &mask)),
        "indirect_diffuse_colour": colour_error(mean_rgb(frame_indirect, &mask), mean_rgb(reference_diffuse, &mask)),
        "beauty_colour": colour_error(mean_rgb(frame_beauty, &mask), mean_rgb(reference_beauty, &mask)),
    });
    RegionScores {
        cells: cs.len(),
        json,
    }
}

/// A straight border on a region: the line `along = at` (an in-plane coordinate, `along_axis` 0
/// for `a`, 1 for `b`) over `[from, to]` of the other in-plane coordinate.
#[derive(Clone, Copy, Debug)]
pub struct ScreenBorder {
    /// Name.
    pub name: &'static str,
    /// The region it lies on.
    pub region: Region,
    /// Which in-plane coordinate is constant along the border (0 `a`, 1 `b`).
    pub across: usize,
    /// Its value on the border.
    pub at: f32,
    /// The run of the other coordinate.
    pub run: (f32, f32),
}

/// Seam step across `border` (module docs) for `frame` and `reference` (luminance), with the
/// half-metre runs both sides of which hold at least `min_pixels` pixels in each image.
pub fn seam_step(
    border: &ScreenBorder,
    hits: &[Option<(Vec3, Vec3)>],
    frame: &[[f32; 3]],
    reference: &[[f32; 3]],
    min_pixels: u32,
) -> Value {
    const STRIP: f32 = 0.5;
    let runs = ((border.run.1 - border.run.0) / STRIP).floor().max(0.0) as usize;
    // Per run: [side][image] sums, count.
    let mut acc = vec![([[0.0f64; 2]; 2], [0u32; 2]); runs];
    for (i, h) in hits.iter().enumerate() {
        let Some((a, b)) = border.region.coords(*h) else {
            continue;
        };
        let (c, r) = if border.across == 0 { (a, b) } else { (b, a) };
        let d = c - border.at;
        if d.abs() > STRIP || d == 0.0 || r < border.run.0 {
            continue;
        }
        let k = ((r - border.run.0) / STRIP) as usize;
        if k >= runs {
            continue;
        }
        let side = usize::from(d > 0.0);
        acc[k].0[side][0] += f64::from(lum(frame[i]));
        acc[k].0[side][1] += f64::from(lum(reference[i]));
        acc[k].1[side] += 1;
    }
    let (mut s, mut n) = ([0.0f64; 2], 0u32);
    for (sums, count) in &acc {
        if count[0] < min_pixels || count[1] < min_pixels {
            continue;
        }
        for img in 0..2 {
            let l = sums[0][img] / f64::from(count[0]);
            let r = sums[1][img] / f64::from(count[1]);
            let m = 0.5 * (l + r);
            if m > 0.0 {
                s[img] += (l - r).abs() / m;
            }
        }
        n += 1;
    }
    let step = |k: usize| (n > 0).then(|| (s[k] / f64::from(n)) as f32);
    json!({ "runs": n, "frame": step(0), "reference": step(1) })
}
