// SPDX-License-Identifier: MIT
// Portions ported from xatlas (https://github.com/jpcy/xatlas, commit f700c77), source/xatlas/xatlas.cpp
// (pack::Atlas::packCharts: density estimate, per-chart scale to texels and integer extents, perimeter
// order, brute-force and random chart placement with their extents metric and start-position buckets;
// BitImage::canBlit / dilate; the KISS random generator xatlas takes from Fast-BVH,
// https://github.com/brandonpelfrey/Fast-BVH, Copyright (c) 2012 Brandon Pelfrey, MIT licence).
// Copyright (c) 2018-2020 Jonathan Young. xatlas is derived from thekla_atlas
// (https://github.com/Thekla/thekla_atlas), Copyright (c) 2013 Thekla, Inc and Copyright NVIDIA
// Corporation 2006 -- Ignacio Castano. MIT licence (see docs/ported-sources.toml):
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

//! Chart packing (stage 3 of `generate`).
//!
//! Differences from the upstream code: one fixed-size atlas (no sub-atlases; a density that does
//! not fit is reduced instead, and an automatic density is searched up as well as down); charts are
//! never scaled down individually, so the density stays uniform; a turned chart is rotated by 90°
//! (winding kept) rather than transposed; the bilinear footprint is a conservative rasterization
//! against the 2×2-texel square around each texel centre; and charts keep their padding at the
//! atlas border. The minimum-area bounding box is the rotating-edge search over the convex hull
//! (Freeman & Shapira 1975; hull by Andrew's monotone chain).

use regia_core::glam::DVec2;

use super::param::FlatChart;
use super::{AtlasError, AtlasOptions, AtlasWarning, parallel_map};

/// Row interleave of the collision test.
const ROW_PHASES: u32 = 8;
/// Positions the fallback scan after a failed random search may test (the slide refines the spot).
const FALLBACK_POSITIONS: f64 = 65_536.0;
/// Placement attempts of the random search (xatlas).
const RANDOM_ATTEMPTS: u32 = 4096;
/// Perimeter buckets of the brute-force start position (xatlas).
const PERIMETER_BUCKETS: f64 = 16.0;
/// Utilization xatlas assumes when estimating a density.
const ASSUMED_UTILIZATION: f64 = 0.75;
/// Packing passes before giving up.
const MAX_PASSES: u32 = 24;
/// Bisection passes between a density that fits and one that does not.
const BISECT_PASSES: u32 = 6;
/// Density step while searching up / down.
const GROW_STEP: f64 = 1.25;
const SHRINK_STEP: f64 = 0.8;
/// Density step when a fixed density does not fit.
const FIXED_SHRINK_STEP: f64 = 0.9;

/// One placed chart.
pub(super) struct PlacedChart {
    /// Atlas texel coordinates of each local vertex.
    pub uvs: Vec<[f64; 2]>,
    /// Turned by 90°.
    pub rotated: bool,
    /// `[x, y, width, height]` of the padded footprint image.
    pub rect: [u32; 4],
}

/// The packing result.
pub(super) struct Packed {
    pub charts: Vec<PlacedChart>,
    pub texels_per_unit: f32,
    pub utilization: f32,
    pub passes: u32,
}

/// A 1-bit image, rows of 64-bit words (bit `x % 64` of word `x / 64`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BitImage {
    width: u32,
    height: u32,
    stride: usize,
    words: Vec<u64>,
}

impl BitImage {
    pub fn new(width: u32, height: u32) -> Self {
        let stride = (width as usize).div_ceil(64).max(1);
        Self {
            width,
            height,
            stride,
            words: vec![0; stride * height as usize],
        }
    }

    fn from_mask(width: u32, height: u32, mask: &[bool]) -> Self {
        let mut image = Self::new(width, height);
        for (i, _) in mask.iter().enumerate().filter(|(_, b)| **b) {
            image.set(i as u32 % width, i as u32 / width);
        }
        image
    }

    pub fn get(&self, x: u32, y: u32) -> bool {
        (self.words[y as usize * self.stride + x as usize / 64] >> (x % 64)) & 1 == 1
    }

    pub fn set(&mut self, x: u32, y: u32) {
        self.words[y as usize * self.stride + x as usize / 64] |= 1 << (x % 64);
    }

    pub fn count_ones(&self) -> u64 {
        self.words.iter().map(|w| u64::from(w.count_ones())).sum()
    }

    /// The image turned by 90° (texel `(x, y)` goes to `(height - 1 - y, x)`).
    fn rotated(&self) -> Self {
        let mut out = Self::new(self.height, self.width);
        for y in 0..self.height {
            for x in 0..self.width {
                if self.get(x, y) {
                    out.set(self.height - 1 - y, x);
                }
            }
        }
        out
    }

    /// The 64 bits of row `y` starting at bit `x0` (zeros past the row end).
    fn window(&self, y: u32, x0: usize) -> u64 {
        let row = y as usize * self.stride;
        let (word, shift) = (x0 / 64, x0 % 64);
        let lo = if word < self.stride {
            self.words[row + word] >> shift
        } else {
            0
        };
        let hi = if shift != 0 && word + 1 < self.stride {
            self.words[row + word + 1] << (64 - shift)
        } else {
            0
        };
        lo | hi
    }

    /// Whether `image` placed at `(ox, oy)` lies inside and touches no set bit.
    fn can_blit(&self, image: &BitImage, ox: u32, oy: u32) -> bool {
        if ox + image.width > self.width || oy + image.height > self.height {
            return false;
        }
        // Every eighth row first: a collision is usually found after a few rows, not a full scan.
        for phase in 0..ROW_PHASES.min(image.height) {
            for y in (phase..image.height).step_by(ROW_PHASES as usize) {
                let row = y as usize * image.stride;
                for k in 0..image.stride {
                    let bits = image.words[row + k];
                    if bits != 0 && self.window(oy + y, ox as usize + k * 64) & bits != 0 {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// ORs `image` in at `(ox, oy)` (the caller checked it fits).
    fn blit(&mut self, image: &BitImage, ox: u32, oy: u32) {
        for y in 0..image.height {
            let src = y as usize * image.stride;
            let dst = (oy + y) as usize * self.stride;
            for k in 0..image.stride {
                let bits = image.words[src + k];
                if bits == 0 {
                    continue;
                }
                let x0 = ox as usize + k * 64;
                let (word, shift) = (x0 / 64, x0 % 64);
                self.words[dst + word] |= bits << shift;
                if shift != 0 && word + 1 < self.stride {
                    self.words[dst + word + 1] |= bits >> (64 - shift);
                }
            }
        }
    }
}

/// Whether triangle `t` overlaps the open square of half-size `half` around `c` (separating axes:
/// the square's two axes and the triangle's three edge normals).
fn triangle_overlaps_square(t: &[DVec2; 3], c: DVec2, half: f64) -> bool {
    let lo = t[0].min(t[1]).min(t[2]);
    let hi = t[0].max(t[1]).max(t[2]);
    if hi.x <= c.x - half || lo.x >= c.x + half || hi.y <= c.y - half || lo.y >= c.y + half {
        return false;
    }
    for i in 0..3 {
        let edge = t[(i + 1) % 3] - t[i];
        let n = DVec2::new(-edge.y, edge.x);
        if n == DVec2::ZERO {
            continue;
        }
        let p = [n.dot(t[0]), n.dot(t[1]), n.dot(t[2])];
        let (tmin, tmax) = (p[0].min(p[1]).min(p[2]), p[0].max(p[1]).max(p[2]));
        let centre = n.dot(c);
        let radius = half * (n.x.abs() + n.y.abs());
        if tmax <= centre - radius || tmin >= centre + radius {
            return false;
        }
    }
    true
}

/// Marks every texel of a `width × height` mask whose square of half-size `half` (0.5 = the texel
/// itself) overlaps a triangle.
pub(super) fn rasterize(
    mask: &mut [bool],
    width: u32,
    height: u32,
    uvs: &[DVec2],
    faces: &[[u32; 3]],
    half: f64,
) {
    for tri in faces {
        let t = tri.map(|v| uvs[v as usize]);
        let lo = t[0].min(t[1]).min(t[2]);
        let hi = t[0].max(t[1]).max(t[2]);
        let x0 = (lo.x - 0.5 - half).floor().max(0.0) as u32;
        let y0 = (lo.y - 0.5 - half).floor().max(0.0) as u32;
        let x1 = ((hi.x - 0.5 + half).ceil().max(0.0) as u32).min(width.saturating_sub(1));
        let y1 = ((hi.y - 0.5 + half).ceil().max(0.0) as u32).min(height.saturating_sub(1));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let index = (y * width + x) as usize;
                if !mask[index]
                    && triangle_overlaps_square(&t, DVec2::new(f64::from(x) + 0.5, f64::from(y) + 0.5), half)
                {
                    mask[index] = true;
                }
            }
        }
    }
}

/// 8-neighbour dilation by `radius` texels (a separable box max).
fn dilate(mask: &[bool], width: u32, height: u32, radius: u32) -> Vec<bool> {
    if radius == 0 {
        return mask.to_vec();
    }
    let (w, h, r) = (width as usize, height as usize, radius as usize);
    let mut horizontal = vec![false; mask.len()];
    for y in 0..h {
        let row = &mask[y * w..(y + 1) * w];
        for x in 0..w {
            let lo = x.saturating_sub(r);
            let hi = (x + r).min(w - 1);
            horizontal[y * w + x] = row[lo..=hi].iter().any(|&b| b);
        }
    }
    let mut out = vec![false; mask.len()];
    for x in 0..w {
        for y in 0..h {
            let lo = y.saturating_sub(r);
            let hi = (y + r).min(h - 1);
            out[y * w + x] = (lo..=hi).any(|yy| horizontal[yy * w + x]);
        }
    }
    out
}

/// Convex hull (Andrew's monotone chain), counter-clockwise, without collinear points.
fn convex_hull(points: &[DVec2]) -> Vec<DVec2> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    sorted.dedup();
    if sorted.len() < 3 {
        return sorted;
    }
    let cross = |o: DVec2, a: DVec2, b: DVec2| (a - o).perp_dot(b - o);
    let mut hull: Vec<DVec2> = Vec::with_capacity(sorted.len() * 2);
    for &p in &sorted {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    let lower = hull.len() + 1;
    for &p in sorted.iter().rev().skip(1) {
        while hull.len() >= lower && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop();
    hull
}

/// Unit direction of the minimum-area bounding rectangle's long side.
fn min_area_axis(points: &[DVec2]) -> DVec2 {
    let hull = convex_hull(points);
    if hull.len() < 3 {
        return DVec2::X;
    }
    let mut best = (f64::MAX, DVec2::X);
    for i in 0..hull.len() {
        let edge = hull[(i + 1) % hull.len()] - hull[i];
        let Some(axis) = edge.try_normalize() else {
            continue;
        };
        let perp = axis.perp();
        let (mut lo, mut hi) = (DVec2::splat(f64::MAX), DVec2::splat(f64::MIN));
        for p in &hull {
            let q = DVec2::new(p.dot(axis), p.dot(perp));
            lo = lo.min(q);
            hi = hi.max(q);
        }
        let size = hi - lo;
        let area = size.x * size.y;
        if area < best.0 {
            best = (area, if size.x >= size.y { axis } else { perp });
        }
    }
    best.1
}

/// A chart turned to its axis and moved to the origin, ready to scale.
struct Aligned {
    uvs: Vec<DVec2>,
    /// Surface units per parametric unit (so `scale · texels_per_unit` gives texels).
    scale: f64,
}

fn align(chart: &FlatChart, to_axis: bool) -> Aligned {
    let axis = if to_axis {
        min_area_axis(&chart.uvs)
    } else {
        DVec2::X
    };
    let perp = axis.perp();
    let mut uvs: Vec<DVec2> = chart
        .uvs
        .iter()
        .map(|p| DVec2::new(p.dot(axis), p.dot(perp)))
        .collect();
    let lo = uvs.iter().fold(DVec2::splat(f64::MAX), |a, p| a.min(*p));
    for p in &mut uvs {
        *p -= lo;
    }
    let param_area: f64 = chart
        .local_faces
        .iter()
        .map(|t| {
            let (a, b, c) = (uvs[t[0] as usize], uvs[t[1] as usize], uvs[t[2] as usize]);
            0.5 * (b - a).perp_dot(c - a).abs()
        })
        .sum();
    let scale = if param_area > 0.0 && chart.surface_area > 0.0 {
        (chart.surface_area / param_area).sqrt()
    } else {
        1.0
    };
    Aligned { uvs, scale }
}

/// One chart rasterized at a density.
struct ChartImage {
    /// Chart texel coordinates (padding offset included).
    uvs: Vec<DVec2>,
    raw: BitImage,
    padded: BitImage,
    raw_rotated: Option<BitImage>,
    padded_rotated: Option<BitImage>,
    perimeter: u32,
}

fn chart_image(chart: &FlatChart, aligned: &Aligned, tpu: f64, o: &AtlasOptions) -> Option<ChartImage> {
    let s = aligned.scale * tpu;
    let mut uvs: Vec<DVec2> = aligned.uvs.iter().map(|p| *p * s).collect();
    let extent = uvs.iter().fold(DVec2::ZERO, |a, p| a.max(*p));
    // Stretch to whole texels (xatlas: boundary vertices land nearer texel centres).
    let least = f64::from(o.min_chart_texels);
    let whole = DVec2::new(extent.x.ceil().max(least), extent.y.ceil().max(least));
    let stretch = DVec2::new(
        if extent.x > 0.0 { whole.x / extent.x } else { 1.0 },
        if extent.y > 0.0 { whole.y / extent.y } else { 1.0 },
    );
    let pad = f64::from(o.padding);
    for p in &mut uvs {
        *p = *p * stretch + DVec2::splat(0.5 + pad);
    }
    let width = whole.x as u64 + 2 * u64::from(o.padding) + 2;
    let height = whole.y as u64 + 2 * u64::from(o.padding) + 2;
    let resolution = u64::from(o.resolution);
    if width > resolution || height > resolution {
        return None;
    }
    let (width, height) = (width as u32, height as u32);
    let mut raw_mask = vec![false; (width * height) as usize];
    rasterize(&mut raw_mask, width, height, &uvs, &chart.local_faces, 0.5);
    let footprint = if o.bilinear {
        let mut mask = raw_mask.clone();
        rasterize(&mut mask, width, height, &uvs, &chart.local_faces, 1.0);
        mask
    } else {
        raw_mask.clone()
    };
    let padded_mask = dilate(&footprint, width, height, o.padding);
    let raw = BitImage::from_mask(width, height, &raw_mask);
    let padded = BitImage::from_mask(width, height, &padded_mask);
    let (raw_rotated, padded_rotated) = if o.rotate_charts {
        (Some(raw.rotated()), Some(padded.rotated()))
    } else {
        (None, None)
    };
    Some(ChartImage {
        uvs,
        raw,
        padded,
        raw_rotated,
        padded_rotated,
        perimeter: width + height,
    })
}

/// KISS generator (Marsaglia), seeded as upstream; `range(n)` is in `0..=n`.
struct Kiss {
    x: u32,
    y: u32,
    z: u32,
    c: u32,
}

impl Kiss {
    fn new() -> Self {
        Self {
            x: 123_456_789,
            y: 362_436_000,
            z: 521_288_629,
            c: 7_654_321,
        }
    }

    fn range(&mut self, range: u32) -> u32 {
        if range == 0 {
            return 0;
        }
        self.x = self.x.wrapping_mul(69_069).wrapping_add(12_345);
        self.y ^= self.y << 13;
        self.y ^= self.y >> 17;
        self.y ^= self.y << 5;
        let t = 698_769_069u64 * u64::from(self.z) + u64::from(self.c);
        self.c = (t >> 32) as u32;
        self.z = t as u32;
        self.x.wrapping_add(self.y).wrapping_add(self.z) % (range.wrapping_add(1).max(1))
    }
}

/// `begin, begin + step, …` up to `end`, always ending with `end` itself (the frontier where the
/// used area grows).
fn stepped(begin: u32, end: u32, step: usize) -> impl Iterator<Item = u32> {
    let last_regular = (begin..=end).step_by(step).last();
    (begin..=end)
        .step_by(step)
        .chain((last_regular != Some(end) && begin <= end).then_some(end))
}

/// A placement: position, footprint size and turn.
#[derive(Clone, Copy)]
struct Spot {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rotated: bool,
}

struct Packer<'a> {
    o: &'a AtlasOptions,
    atlas: BitImage,
    coverage: BitImage,
    used_w: u32,
    used_h: u32,
    start: (u32, u32),
    rng: Kiss,
}

impl Packer<'_> {
    fn metric(&self, x: u32, y: u32, w: u32, h: u32) -> (u64, u64) {
        let ex = u64::from(self.used_w.max(x + w));
        let ey = u64::from(self.used_h.max(y + h));
        let extent = ex.max(ey);
        (extent * extent + ex * ey, ex * ey)
    }

    fn orientations<'i>(&self, image: &'i ChartImage) -> Vec<(&'i BitImage, bool)> {
        let mut out = vec![(&image.padded, false)];
        if let Some(rotated) = &image.padded_rotated {
            out.push((rotated, true));
        }
        out
    }

    fn brute_force(&self, image: &ChartImage, step: u32) -> Option<Spot> {
        let res = self.o.resolution;
        let step = step.max(1) as usize;
        let mut best: Option<(u64, Spot)> = None;
        for (bits, rotated) in self.orientations(image) {
            let (cw, ch) = (bits.width, bits.height);
            if cw > res || ch > res {
                continue;
            }
            let y_end = (self.used_h + 1).min(res - ch);
            let x_end = (self.used_w + 1).min(res - cw);
            if self.start.1 > y_end {
                continue;
            }
            for y in stepped(self.start.1, y_end, step) {
                let x_begin = if y == self.start.1 {
                    self.start.0.min(x_end)
                } else {
                    0
                };
                for x in stepped(x_begin, x_end, step) {
                    let (metric, area) = self.metric(x, y, cw, ch);
                    if let Some((b, spot)) = best
                        && (metric > b || (metric == b && x.max(y) >= spot.x.max(spot.y)))
                    {
                        continue;
                    }
                    if !self.atlas.can_blit(bits, x, y) {
                        continue;
                    }
                    best = Some((
                        metric,
                        Spot {
                            x,
                            y,
                            w: cw,
                            h: ch,
                            rotated,
                        },
                    ));
                    if area == u64::from(self.used_w) * u64::from(self.used_h) {
                        return best.map(|(_, s)| s);
                    }
                }
            }
        }
        best.map(|(_, s)| s)
    }

    fn random(&mut self, image: &ChartImage) -> Option<Spot> {
        let res = self.o.resolution;
        let mut best: Option<(u64, Spot)> = None;
        for _ in 0..RANDOM_ATTEMPTS {
            let turn = image.padded_rotated.is_some() && self.rng.range(1) == 1;
            let bits = match (&image.padded_rotated, turn) {
                (Some(rotated), true) => rotated,
                _ => &image.padded,
            };
            let (cw, ch) = (bits.width, bits.height);
            if cw > res || ch > res {
                continue;
            }
            let x_range = (self.used_w + 1).min(res - cw);
            let y_range = (self.used_h + 1).min(res - ch);
            let x = self.rng.range(x_range);
            let y = self.rng.range(y_range);
            let (metric, area) = self.metric(x, y, cw, ch);
            if let Some((b, spot)) = best
                && (metric > b || (metric == b && x.min(y) > spot.x.min(spot.y)))
            {
                continue;
            }
            if !self.atlas.can_blit(bits, x, y) {
                continue;
            }
            best = Some((
                metric,
                Spot {
                    x,
                    y,
                    w: cw,
                    h: ch,
                    rotated: turn,
                },
            ));
            if area == u64::from(self.used_w) * u64::from(self.used_h) {
                break;
            }
        }
        best.map(|(_, s)| s)
    }

    fn find(&mut self, image: &ChartImage) -> Option<Spot> {
        let used = u64::from(self.used_w) * u64::from(self.used_h);
        // An empty or small atlas: every position is cheap to try.
        let exhaustive = self.o.brute_force || u64::from(RANDOM_ATTEMPTS) >= used;
        let first = if exhaustive {
            self.brute_force(image, 1)
        } else {
            self.random(image)
        };
        let found = first.or_else(|| {
            // The start position may have skipped the only free spot, or random sampling missed
            // it: scan everything (coarsely after a random search) before declaring it full.
            let step = if exhaustive {
                1
            } else {
                let res = f64::from(self.o.resolution);
                let side = f64::from(image.padded.width.min(image.padded.height));
                let positions = (res - side + 1.0).max(1.0).powi(2);
                (positions / FALLBACK_POSITIONS).sqrt().ceil() as u32
            };
            let start = std::mem::take(&mut self.start);
            let found = self.brute_force(image, step);
            if found.is_none() {
                self.start = start;
            }
            found
        })?;
        Some(self.slide(image, found))
    }

    /// Moves a free spot up, then left, while it stays free (bottom-left compaction toward the
    /// origin; the extents metric can only improve).
    fn slide(&self, image: &ChartImage, mut spot: Spot) -> Spot {
        let bits = match (&image.padded_rotated, spot.rotated) {
            (Some(rotated), true) => rotated,
            _ => &image.padded,
        };
        loop {
            let (x0, y0) = (spot.x, spot.y);
            while spot.y > 0 && self.atlas.can_blit(bits, spot.x, spot.y - 1) {
                spot.y -= 1;
            }
            while spot.x > 0 && self.atlas.can_blit(bits, spot.x - 1, spot.y) {
                spot.x -= 1;
            }
            if (spot.x, spot.y) == (x0, y0) {
                return spot;
            }
        }
    }
}

/// One packing pass at density `tpu`; `None` when a chart does not fit.
fn pass(
    flat: &[FlatChart],
    aligned: &[Aligned],
    tpu: f64,
    o: &AtlasOptions,
    threads: usize,
) -> Option<Packed> {
    let _span = tracing::debug_span!("atlas.pack_pass", tpu).entered();
    let images: Vec<Option<ChartImage>> = parallel_map(flat.len(), threads, |i| {
        chart_image(&flat[i], &aligned[i], tpu, o)
    });
    let images: Vec<ChartImage> = images.into_iter().collect::<Option<Vec<_>>>()?;
    let mut order: Vec<usize> = (0..images.len()).collect();
    order.sort_by(|&a, &b| images[b].perimeter.cmp(&images[a].perimeter).then(a.cmp(&b)));
    let (min_perimeter, max_perimeter) = images.iter().fold((u32::MAX, 0), |(lo, hi), im| {
        (lo.min(im.perimeter), hi.max(im.perimeter))
    });
    let bucket = f64::from(max_perimeter.saturating_sub(min_perimeter)) / PERIMETER_BUCKETS;
    let mut current_bucket = 0.0;
    let mut packer = Packer {
        o,
        atlas: BitImage::new(o.resolution, o.resolution),
        coverage: BitImage::new(o.resolution, o.resolution),
        used_w: 0,
        used_h: 0,
        start: (0, 0),
        rng: Kiss::new(),
    };
    let mut spots: Vec<Option<Spot>> = vec![None; images.len()];
    for &c in &order {
        let image = &images[c];
        if o.brute_force {
            let p = f64::from(image.perimeter);
            if p > f64::from(min_perimeter) && p <= f64::from(max_perimeter) - bucket * (current_bucket + 1.0)
            {
                packer.start = (0, 0);
                current_bucket += 1.0;
            }
        }
        let spot = packer.find(image)?;
        let (bits, raw) = if spot.rotated {
            (image.padded_rotated.as_ref()?, image.raw_rotated.as_ref()?)
        } else {
            (&image.padded, &image.raw)
        };
        packer.atlas.blit(bits, spot.x, spot.y);
        packer.coverage.blit(raw, spot.x, spot.y);
        if spot.x + spot.w > packer.used_w || spot.y + spot.h > packer.used_h {
            packer.start = (0, 0);
        } else {
            packer.start = (spot.x, spot.y);
        }
        packer.used_w = packer.used_w.max(spot.x + spot.w);
        packer.used_h = packer.used_h.max(spot.y + spot.h);
        spots[c] = Some(spot);
    }
    let mut charts = Vec::with_capacity(images.len());
    for (image, spot) in images.iter().zip(&spots) {
        let spot = (*spot)?;
        let rows = f64::from(image.padded.height);
        let uvs = image
            .uvs
            .iter()
            .map(|p| {
                let local = if spot.rotated {
                    DVec2::new(rows - p.y, p.x)
                } else {
                    *p
                };
                [local.x + f64::from(spot.x), local.y + f64::from(spot.y)]
            })
            .collect();
        charts.push(PlacedChart {
            uvs,
            rotated: spot.rotated,
            rect: [spot.x, spot.y, spot.w, spot.h],
        });
    }
    let total = f64::from(o.resolution) * f64::from(o.resolution);
    Some(Packed {
        charts,
        texels_per_unit: tpu as f32,
        utilization: (packer.coverage.count_ones() as f64 / total) as f32,
        passes: 0,
    })
}

/// One packing pass at exactly `o.texels_per_unit`: `None` when the charts do not fit.
pub(super) fn pack_exact(flat: &[FlatChart], o: &AtlasOptions, threads: usize) -> Option<Packed> {
    let _span = tracing::debug_span!("atlas.pack_exact", charts = flat.len()).entered();
    let aligned: Vec<Aligned> =
        parallel_map(flat.len(), threads, |i| align(&flat[i], o.rotate_charts_to_axis));
    let mut packed = pass(flat, &aligned, f64::from(o.texels_per_unit), o, threads)?;
    packed.passes = 1;
    Some(packed)
}

/// Packs every chart into one `resolution²` atlas (see the module docs for the density search).
pub(super) fn pack(
    flat: &[FlatChart],
    o: &AtlasOptions,
    threads: usize,
    warnings: &mut Vec<AtlasWarning>,
) -> Result<Packed, AtlasError> {
    let _span = tracing::debug_span!("atlas.pack", charts = flat.len()).entered();
    let aligned: Vec<Aligned> =
        parallel_map(flat.len(), threads, |i| align(&flat[i], o.rotate_charts_to_axis));
    let surface: f64 = flat.iter().map(|c| c.surface_area).sum();
    let res = f64::from(o.resolution);
    let mut passes = 0u32;
    let run = |tpu: f64, passes: &mut u32| {
        *passes += 1;
        pass(flat, &aligned, tpu, o, threads)
    };
    let does_not_fit = AtlasError::DoesNotFit {
        resolution: o.resolution,
    };
    let estimate = if surface > 0.0 {
        res * (ASSUMED_UTILIZATION / surface).sqrt()
    } else {
        1.0
    };
    if o.texels_per_unit > 0.0 {
        let requested = f64::from(o.texels_per_unit);
        let mut tpu = requested;
        while passes < MAX_PASSES {
            if let Some(mut packed) = run(tpu, &mut passes) {
                if tpu < requested {
                    warnings.push(AtlasWarning::DensityReduced {
                        requested: o.texels_per_unit,
                        used: tpu as f32,
                    });
                }
                packed.passes = passes;
                return Ok(packed);
            }
            // After the first miss, jump to the density that would fill the atlas when lower.
            tpu = if tpu == requested {
                (tpu * FIXED_SHRINK_STEP).min(estimate)
            } else {
                tpu * FIXED_SHRINK_STEP
            };
        }
        return Err(does_not_fit);
    }
    let mut fit: Option<(f64, Packed)> = None;
    let mut miss: Option<f64> = None;
    let mut tpu = estimate;
    // Bracket: grow while it fits, or shrink until it does.
    while passes < MAX_PASSES {
        match run(tpu, &mut passes) {
            Some(packed) => {
                fit = Some((tpu, packed));
                if miss.is_some() {
                    break;
                }
                tpu *= GROW_STEP;
            }
            None => {
                miss = Some(tpu);
                if fit.is_some() {
                    break;
                }
                tpu *= SHRINK_STEP;
            }
        }
    }
    let Some((mut lo, mut best)) = fit else {
        return Err(does_not_fit);
    };
    if let Some(mut hi) = miss {
        for _ in 0..BISECT_PASSES {
            if passes >= MAX_PASSES {
                break;
            }
            let mid = (lo * hi).sqrt();
            match run(mid, &mut passes) {
                Some(packed) => {
                    lo = mid;
                    best = packed;
                }
                None => hi = mid,
            }
        }
    }
    best.passes = passes;
    Ok(best)
}

#[cfg(test)]
mod bit_image_tests {
    use super::*;

    fn solid(w: u32, h: u32) -> BitImage {
        let mut image = BitImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                image.set(x, y);
            }
        }
        image
    }

    #[test]
    fn an_exact_fit_is_accepted_and_one_texel_more_is_refused() {
        let page = BitImage::new(100, 70);
        assert!(page.can_blit(&solid(100, 70), 0, 0), "whole page");
        assert!(!page.can_blit(&solid(101, 70), 0, 0), "one column too wide");
        assert!(!page.can_blit(&solid(100, 71), 0, 0), "one row too tall");
        assert!(
            page.can_blit(&solid(36, 10), 64, 60),
            "flush with the corner, across a word edge"
        );
        assert!(!page.can_blit(&solid(37, 10), 64, 60), "past the right edge");
    }

    #[test]
    fn an_item_larger_than_the_page_never_fits_and_zero_size_always_does() {
        let page = BitImage::new(64, 64);
        assert!(!page.can_blit(&solid(65, 1), 0, 0));
        assert!(!page.can_blit(&solid(1, 65), 0, 0));
        assert!(!page.can_blit(&solid(200, 200), 0, 0));
        let empty = BitImage::new(0, 0);
        assert!(page.can_blit(&empty, 0, 0));
        assert!(
            page.can_blit(&empty, 64, 64),
            "a zero-size item on the far corner is inside"
        );
        let mut copy = page.clone();
        copy.blit(&empty, 3, 3);
        assert_eq!(copy, page, "blitting nothing changes nothing");
        let zero_page = BitImage::new(0, 0);
        assert!(!zero_page.can_blit(&solid(1, 1), 0, 0));
    }

    #[test]
    fn blit_across_word_boundaries_sets_exactly_the_item_texels_and_collides_after() {
        for ox in [0u32, 1, 31, 63, 64, 65, 100] {
            let mut page = BitImage::new(192, 20);
            let item = solid(70, 5);
            assert!(page.can_blit(&item, ox, 3), "ox {ox}");
            page.blit(&item, ox, 3);
            assert_eq!(page.count_ones(), 70 * 5, "ox {ox}");
            for y in 0..20 {
                for x in 0..192 {
                    let inside = (ox..ox + 70).contains(&x) && (3..8).contains(&y);
                    assert_eq!(page.get(x, y), inside, "ox {ox} at ({x},{y})");
                }
            }
            assert!(!page.can_blit(&solid(1, 1), ox + 69, 7), "last texel is taken");
            assert!(page.can_blit(&solid(1, 1), ox + 70, 7), "the next texel is free");
            assert!(page.can_blit(&solid(70, 1), ox, 8), "the row below is free");
        }
    }

    #[test]
    fn a_hollow_item_lets_another_nest_inside_and_rotation_is_exact() {
        let mut page = BitImage::new(32, 32);
        let mut ring = BitImage::new(10, 10);
        for i in 0..10 {
            for (x, y) in [(i, 0), (i, 9), (0, i), (9, i)] {
                ring.set(x, y);
            }
        }
        page.blit(&ring, 5, 5);
        assert!(page.can_blit(&solid(8, 8), 6, 6), "fits in the hole");
        assert!(!page.can_blit(&solid(9, 9), 6, 6), "touches the ring");

        let mut image = BitImage::new(3, 2);
        image.set(0, 0);
        image.set(2, 1);
        let turned = image.rotated();
        assert_eq!((turned.width, turned.height), (2, 3));
        assert!(turned.get(1, 0), "(0,0) goes to (h-1-0, 0)");
        assert!(turned.get(0, 2), "(2,1) goes to (h-1-1, 2)");
        assert_eq!(turned.count_ones(), 2);
        assert_eq!(
            turned.rotated().rotated().rotated(),
            image,
            "four turns are the identity"
        );
    }

    #[test]
    fn stepped_always_ends_on_the_last_position() {
        assert_eq!(stepped(0, 10, 4).collect::<Vec<_>>(), [0, 4, 8, 10]);
        assert_eq!(stepped(0, 8, 4).collect::<Vec<_>>(), [0, 4, 8]);
        assert_eq!(stepped(5, 5, 3).collect::<Vec<_>>(), [5]);
        assert_eq!(stepped(6, 5, 3).count(), 0, "empty range");
    }
}
