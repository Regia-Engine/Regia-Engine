// SPDX-License-Identifier: MPL-2.0
//! The shore bake (spec 87 § 2, ADR-1828): a signed distance to the shoreline and the water depth
//! per texel over a plane body, from any bed (a [`crate::TerrainMap`] or a height function), and
//! the shoaling and breaking it drives (spec 87 § 3).
//!
//! The distance is the exact Euclidean distance transform of the wet and the dry texels by the
//! separable lower envelope of parabolas (Felzenszwalb and Huttenlocher, "Distance Transforms of
//! Sampled Functions", Theory of Computing 8, 2012). Shoaling is linear wave theory's
//! energy-flux coefficient (Dean and Dalrymple, *Water Wave Mechanics for Engineers and
//! Scientists*, ch. 4), capped by McCowan's (1894) breaking index.
//!
//! A [`ShoreField`] is a derived cache, not content: a reader that finds another
//! [`SHORE_FORMAT_VERSION`] refuses it ([`ShoreError::Version`]) and the caller bakes again.

use std::sync::Arc;

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use crate::sampler::TerrainMap;

/// Version of the serialized [`ShoreField`] ([`ShoreField::to_bytes`]).
pub const SHORE_FORMAT_VERSION: u32 = 1;

/// Magic bytes in front of a serialized [`ShoreField`].
pub const SHORE_MAGIC: [u8; 4] = *b"RGSH";

/// Most texels along either side of a field; the cell grows until the rectangle fits.
pub const MAX_SHORE_SIZE: u32 = 1024;

/// Smallest texel (m).
pub const MIN_SHORE_CELL: f32 = 0.25;

/// McCowan's breaking index: a wave breaks where its height reaches this fraction of the depth.
pub const BREAKING_INDEX: f32 = 0.78;

/// Largest shoaling scale on a wave's amplitude.
pub const SHOAL_MAX: f32 = 2.0;

/// `k h` at and above which the water counts as deep (the shoaling scale is 1): one wavelength of
/// depth, where the coefficient is within 5e-5 of 1 (at half a wavelength it is still 1 % off, a
/// visible step).
pub const DEEP_KH: f32 = std::f32::consts::TAU;

/// Smallest `k h` the shoaling coefficient is evaluated at.
pub const MIN_KH: f32 = 1.0e-4;

/// The depth a point outside every shore field counts as (deep water: waves unscaled); the GPU
/// pads the shore texture with it.
pub const SHORE_DEEP: f32 = 1.0e30;

/// Breaker foam starts where a wave's shoaled amplitude reaches this fraction of the breaking cap.
pub const BREAKER_ONSET: f32 = 0.7;

/// Errors from the shore bake and its reader.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ShoreError {
    /// A level, corner or cell is NaN or infinite.
    #[error("{0} is not finite")]
    NotFinite(&'static str),
    /// The rectangle is empty or inverted.
    #[error("the shore rectangle is empty (min {min:?}, max {max:?})")]
    EmptyRect {
        /// Smallest x and z.
        min: [f32; 2],
        /// Largest x and z.
        max: [f32; 2],
    },
    /// The bytes do not start with [`SHORE_MAGIC`].
    #[error("not a shore field (bad magic)")]
    BadMagic,
    /// The bytes were written by another format version: bake again.
    #[error("shore field version {found}, this build reads {expected}: bake it again")]
    Version {
        /// Version in the bytes.
        found: u32,
        /// [`SHORE_FORMAT_VERSION`].
        expected: u32,
    },
    /// The payload does not decode or its arrays do not match its size.
    #[error("corrupt shore field: {0}")]
    Corrupt(String),
}

/// What to bake: the water level and the rectangle (world x and z) the field covers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShoreBakeDesc {
    /// Water level (m).
    pub level: f32,
    /// Smallest x and z (m).
    pub min: [f32; 2],
    /// Largest x and z (m).
    pub max: [f32; 2],
    /// Wanted texel (m); raised to [`MIN_SHORE_CELL`] and until [`MAX_SHORE_SIZE`] fits.
    pub cell: f32,
}

/// The bed under one point of a [`ShoreField`] ([`ShoreField::sample`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ShoreSample {
    /// Signed horizontal distance to the shoreline (m): positive in the water, negative on land.
    pub distance: f32,
    /// Water depth (m): the level minus the bed; negative on land.
    pub depth: f32,
}

/// Summary numbers of a field (agent `water.state`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ShoreStats {
    /// Texels along x and z.
    pub size: [u32; 2],
    /// Texel (m).
    pub cell: f32,
    /// Level the field was baked for (m).
    pub level: f32,
    /// Fraction of texels in the water.
    pub wet_fraction: f32,
    /// Deepest texel (m).
    pub max_depth: f32,
    /// Wet texels with a dry 4-neighbor.
    pub shoreline_texels: u32,
}

/// Signed shoreline distance and depth on a regular grid (spec 87 § 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShoreField {
    /// [`SHORE_FORMAT_VERSION`] when baked.
    pub format_version: u32,
    /// World x and z of texel (0, 0)'s center.
    pub origin: [f32; 2],
    /// Texel (m).
    pub cell: f32,
    /// Texels along x and z.
    pub size: [u32; 2],
    /// Water level the field was baked for (m).
    pub level: f32,
    /// Signed distance to the shoreline per texel, row-major (z rows), m: + water, − land.
    pub distance: Vec<f32>,
    /// Level minus bed per texel, row-major, m: + water, − land.
    pub depth: Vec<f32>,
}

/// The shore field of a plane body, on the body's entity (spec 87 § 2.2).
#[derive(Component, Debug, Clone)]
pub struct WaterShore {
    /// The baked field (shared with the GPU upload).
    pub field: Arc<ShoreField>,
}

impl ShoreField {
    /// Bakes a field over `desc`'s rectangle from `bed(x, z)` (the bed height, m).
    ///
    /// # Errors
    ///
    /// [`ShoreError::NotFinite`] or [`ShoreError::EmptyRect`] for a bad description.
    pub fn bake(desc: &ShoreBakeDesc, bed: impl Fn(f32, f32) -> f32) -> Result<Self, ShoreError> {
        let _span = tracing::debug_span!("water_shore_bake").entered();
        if !desc.level.is_finite() {
            return Err(ShoreError::NotFinite("shore.level"));
        }
        if !desc.min.iter().chain(&desc.max).all(|v| v.is_finite()) {
            return Err(ShoreError::NotFinite("shore.rect"));
        }
        let span = [desc.max[0] - desc.min[0], desc.max[1] - desc.min[1]];
        if !(span[0] > 0.0 && span[1] > 0.0) {
            return Err(ShoreError::EmptyRect {
                min: desc.min,
                max: desc.max,
            });
        }
        let mut cell = if desc.cell.is_finite() {
            desc.cell.max(MIN_SHORE_CELL)
        } else {
            MIN_SHORE_CELL
        };
        let longest = span[0].max(span[1]);
        let fit = longest / (MAX_SHORE_SIZE - 1) as f32;
        if cell < fit {
            cell = fit;
        }
        // Spec 87 § 2.1: the cell is raised until both sides fit. `fit` rounded in f32 can leave
        // `longest / cell` a hair above `MAX_SHORE_SIZE - 1`; clamping the count then would cut
        // the far edge off the field, so step the cell up instead (a few ulps at most).
        while (longest / cell).ceil() as u32 + 1 > MAX_SHORE_SIZE {
            cell = cell.next_up();
        }
        let texels = |s: f32| ((s / cell).ceil() as u32 + 1).max(2);
        let size = [texels(span[0]), texels(span[1])];
        let (w, h) = (size[0] as usize, size[1] as usize);
        let mut depth = Vec::with_capacity(w * h);
        for j in 0..h {
            let z = desc.min[1] + j as f32 * cell;
            for i in 0..w {
                let x = desc.min[0] + i as f32 * cell;
                let b = bed(x, z);
                depth.push(if b.is_finite() { desc.level - b } else { -1.0e6 });
            }
        }
        let distance = signed_distance(&depth, w, h, cell);
        Ok(Self {
            format_version: SHORE_FORMAT_VERSION,
            origin: desc.min,
            cell,
            size,
            level: desc.level,
            distance,
            depth,
        })
    }

    /// Bakes a field from a terrain's bilinear height over `min..max` at the map's sample spacing
    /// (at least [`MIN_SHORE_CELL`]).
    ///
    /// # Errors
    ///
    /// As [`Self::bake`].
    pub fn bake_terrain(
        map: &TerrainMap,
        level: f32,
        min: [f32; 2],
        max: [f32; 2],
    ) -> Result<Self, ShoreError> {
        let spacing = map.world_width() / (map.width().max(2) - 1) as f32;
        Self::bake(
            &ShoreBakeDesc {
                level,
                min,
                max,
                cell: spacing,
            },
            |x, z| map.sample_bilinear(x, z),
        )
    }

    /// Whether the size is at least 2 × 2 and both arrays hold at least `size[0] * size[1]`
    /// texels. The fields are public, so a hand-built or deserialized field may not.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        let (w, h) = (self.size[0] as usize, self.size[1] as usize);
        let n = w.saturating_mul(h);
        w >= 2 && h >= 2 && self.distance.len() >= n && self.depth.len() >= n
    }

    /// The bed at world `(x, z)`, bilinear between texel centers; `None` outside the field.
    #[must_use]
    pub fn sample(&self, x: f32, z: f32) -> Option<ShoreSample> {
        let u = (x - self.origin[0]) / self.cell;
        let v = (z - self.origin[1]) / self.cell;
        let (w, h) = (self.size[0] as usize, self.size[1] as usize);
        // The fields are public: a hand-built field with too few texels or short arrays misses
        // rather than panicking.
        let n = w.saturating_mul(h);
        if w < 2 || h < 2 || self.distance.len() < n || self.depth.len() < n {
            return None;
        }
        let wmax = (w - 1) as f32;
        let hmax = (h - 1) as f32;
        if !(u >= 0.0 && v >= 0.0 && u <= wmax && v <= hmax) {
            return None;
        }
        let i0 = (u.floor() as usize).min(w - 2);
        let j0 = (v.floor() as usize).min(h - 2);
        let fu = u - i0 as f32;
        let fv = v - j0 as f32;
        let lerp = |a: &[f32]| {
            let at = |i: usize, j: usize| a[j * w + i];
            let top = at(i0, j0) + (at(i0 + 1, j0) - at(i0, j0)) * fu;
            let bottom = at(i0, j0 + 1) + (at(i0 + 1, j0 + 1) - at(i0, j0 + 1)) * fu;
            top + (bottom - top) * fv
        };
        Some(ShoreSample {
            distance: lerp(&self.distance),
            depth: lerp(&self.depth),
        })
    }

    /// The field's summary numbers.
    #[must_use]
    pub fn stats(&self) -> ShoreStats {
        let (w, h) = (self.size[0] as usize, self.size[1] as usize);
        if !self.is_consistent() {
            return ShoreStats {
                size: self.size,
                cell: self.cell,
                level: self.level,
                wet_fraction: 0.0,
                max_depth: 0.0,
                shoreline_texels: 0,
            };
        }
        // `is_consistent` allows longer arrays: only the field's own `w * h` texels count.
        let depth = &self.depth[..w * h];
        let wet = depth.iter().filter(|d| **d > 0.0).count();
        let max_depth = depth.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mut shoreline = 0u32;
        for j in 0..h {
            for i in 0..w {
                if self.depth[j * w + i] <= 0.0 {
                    continue;
                }
                let dry = |ii: usize, jj: usize| self.depth[jj * w + ii] <= 0.0;
                if (i > 0 && dry(i - 1, j))
                    || (i + 1 < w && dry(i + 1, j))
                    || (j > 0 && dry(i, j - 1))
                    || (j + 1 < h && dry(i, j + 1))
                {
                    shoreline += 1;
                }
            }
        }
        ShoreStats {
            size: self.size,
            cell: self.cell,
            level: self.level,
            wet_fraction: wet as f32 / (w * h).max(1) as f32,
            max_depth,
            shoreline_texels: shoreline,
        }
    }

    /// The field as bytes: [`SHORE_MAGIC`], [`SHORE_FORMAT_VERSION`] (u32 little endian), then the
    /// postcard payload.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 8 * self.depth.len() + 64);
        out.extend_from_slice(&SHORE_MAGIC);
        out.extend_from_slice(&SHORE_FORMAT_VERSION.to_le_bytes());
        // A `Vec` sink cannot fail; an error here would be a postcard bug, and an empty payload
        // then fails `from_bytes` loudly rather than panicking at runtime.
        if let Ok(payload) = postcard::to_allocvec(self) {
            out.extend_from_slice(&payload);
        }
        out
    }

    /// Reads [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// [`ShoreError::BadMagic`], [`ShoreError::Version`] (bake again) or [`ShoreError::Corrupt`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ShoreError> {
        if bytes.len() < 8 || bytes[..4] != SHORE_MAGIC {
            return Err(ShoreError::BadMagic);
        }
        let found = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if found != SHORE_FORMAT_VERSION {
            return Err(ShoreError::Version {
                found,
                expected: SHORE_FORMAT_VERSION,
            });
        }
        let field: Self =
            postcard::from_bytes(&bytes[8..]).map_err(|e| ShoreError::Corrupt(e.to_string()))?;
        let n = (field.size[0] as usize).saturating_mul(field.size[1] as usize);
        if field.size[0] < 2
            || field.size[1] < 2
            || field.distance.len() != n
            || field.depth.len() != n
            || !(field.cell.is_finite() && field.cell > 0.0)
            || field.format_version != SHORE_FORMAT_VERSION
        {
            return Err(ShoreError::Corrupt("size, cell or arrays do not agree".into()));
        }
        Ok(field)
    }
}

/// Signed distance per texel (spec 87 § 2.1 step 3) from the depths of a `w × h` grid.
fn signed_distance(depth: &[f32], w: usize, h: usize, cell: f32) -> Vec<f32> {
    let wet: Vec<bool> = depth.iter().map(|d| *d > 0.0).collect();
    let any_wet = wet.iter().any(|b| *b);
    let any_dry = wet.iter().any(|b| !*b);
    if !(any_wet && any_dry) {
        let far = w.max(h) as f32 * cell;
        return wet.iter().map(|b| if *b { far } else { -far }).collect();
    }
    // Squared distance (in texels²) to the nearest dry texel, and to the nearest wet texel.
    let to_dry = edt(&wet.iter().map(|b| !*b).collect::<Vec<_>>(), w, h);
    let to_wet = edt(&wet, w, h);
    let half = 0.5 * cell;
    wet.iter()
        .enumerate()
        .map(|(i, b)| {
            if *b {
                to_dry[i].sqrt() * cell - half
            } else {
                -(to_wet[i].sqrt() * cell - half)
            }
        })
        .collect()
}

/// A squared distance larger than any on the grid: texels with no feature in a row keep it until
/// the column pass.
const FAR: f32 = 1.0e20;

/// Squared Euclidean distance (texels²) from every texel to the nearest `feature` texel, by two
/// separable passes of the 1D lower-envelope transform (Felzenszwalb and Huttenlocher 2012).
#[must_use]
pub fn edt(feature: &[bool], w: usize, h: usize) -> Vec<f32> {
    if w == 0 || h == 0 || feature.len() < w.saturating_mul(h) {
        return Vec::new();
    }
    let mut grid: Vec<f32> = feature.iter().map(|f| if *f { 0.0 } else { FAR }).collect();
    let n = w.max(h);
    let mut f = vec![0.0f32; n];
    let mut d = vec![0.0f32; n];
    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];
    for j in 0..h {
        f[..w].copy_from_slice(&grid[j * w..(j + 1) * w]);
        edt_1d(&f[..w], &mut d[..w], &mut v, &mut z);
        grid[j * w..(j + 1) * w].copy_from_slice(&d[..w]);
    }
    for i in 0..w {
        for (j, fj) in f[..h].iter_mut().enumerate() {
            *fj = grid[j * w + i];
        }
        edt_1d(&f[..h], &mut d[..h], &mut v, &mut z);
        for (j, dj) in d[..h].iter().enumerate() {
            grid[j * w + i] = *dj;
        }
    }
    grid
}

/// One 1D pass: `d[q] = min_p ((q − p)² + f[p])`. `v` and `z` are scratch of at least `f.len()`
/// and `f.len() + 1`.
fn edt_1d(f: &[f32], d: &mut [f32], v: &mut [usize], z: &mut [f32]) {
    // Sample positions with an infinite value never enter the envelope; with none finite the
    // output stays FAR.
    let mut k: usize = 0;
    let mut started = false;
    for (q, &fq) in f.iter().enumerate() {
        if fq >= FAR {
            continue;
        }
        if !started {
            v[0] = q;
            z[0] = f32::NEG_INFINITY;
            z[1] = f32::INFINITY;
            started = true;
            continue;
        }
        let qf = q as f32;
        loop {
            let p = v[k];
            let pf = p as f32;
            let s = ((fq + qf * qf) - (f[p] + pf * pf)) / (2.0 * qf - 2.0 * pf);
            // z[0] is −∞, so a finite `s` never pops the first parabola.
            if s <= z[k] && k > 0 {
                k -= 1;
                continue;
            }
            k += 1;
            v[k] = q;
            z[k] = s;
            z[k + 1] = f32::INFINITY;
            break;
        }
    }
    if !started {
        d.fill(FAR);
        return;
    }
    let mut k = 0usize;
    for (q, dq) in d.iter_mut().enumerate() {
        let qf = q as f32;
        while z[k + 1] < qf {
            k += 1;
        }
        let p = v[k];
        let diff = qf - p as f32;
        *dq = diff * diff + f[p];
    }
}

/// Linear theory's shoaling coefficient for wave number `k` (deep water) at depth `h`:
/// `1 / √(tanh(kh) · (1 + 2kh / sinh(2kh)))`, with `kh` clamped to [`MIN_KH`]..; 1 at and above
/// [`DEEP_KH`] (mirrors `water_body.wgsl` `shoal_coefficient`).
#[must_use]
pub fn shoal_coefficient(k: f32, h: f32) -> f32 {
    let kh = (k * h).max(MIN_KH);
    if kh >= DEEP_KH {
        return 1.0;
    }
    let two = 2.0 * kh;
    let n = 1.0 + two / two.sinh();
    1.0 / (kh.tanh() * n).sqrt()
}

/// The amplitude scale of a wave of wave number `k` and amplitude `amplitude` at depth `depth`:
/// shoaled ([`shoal_coefficient`]) and capped at McCowan's limit `γ h / 2`, 0 on land, at most
/// [`SHOAL_MAX`]; and how close the shoaled wave is to breaking (shoaled amplitude over the cap).
/// Mirrors `water_body.wgsl` `shoal_scale`.
#[must_use]
pub fn shoal_scale(k: f32, amplitude: f32, depth: f32) -> (f32, f32) {
    if depth.is_nan() || depth <= 0.0 {
        return (0.0, 1.0);
    }
    let ks = shoal_coefficient(k, depth);
    let cap = 0.5 * BREAKING_INDEX * depth;
    if amplitude.is_nan() || amplitude <= 0.0 {
        return (ks.min(SHOAL_MAX), 0.0);
    }
    let ratio = ks * amplitude / cap;
    (ks.min(cap / amplitude).clamp(0.0, SHOAL_MAX), ratio)
}

/// Breaker foam, 0..=1, from the largest shoaled-amplitude-over-cap ratio of the waves:
/// `smoothstep(BREAKER_ONSET, 1, ratio)` (mirrors `water_body.wgsl` `breaker_foam`).
#[must_use]
pub fn breaker_foam(ratio: f32) -> f32 {
    smoothstep(BREAKER_ONSET, 1.0, ratio)
}

/// Shore foam, 0..=1, over the band `band` meters out from the shoreline:
/// `1 − smoothstep(0, band, distance)` in the water, 0 on land (mirrors `water_body.wgsl`
/// `shore_foam`).
#[must_use]
pub fn shore_foam(distance: f32, band: f32) -> f32 {
    if distance.is_nan() || distance < 0.0 || band.is_nan() || band <= 0.0 {
        return 0.0;
    }
    1.0 - smoothstep(0.0, band, distance)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic pseudo-random mask (xorshift).
    fn mask(w: usize, h: usize, seed: u32, density: u32) -> Vec<bool> {
        let mut s = seed;
        (0..w * h)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s % 100 < density
            })
            .collect()
    }

    /// The separable transform equals brute force on random masks, including empty rows and
    /// columns and a single feature.
    #[test]
    fn edt_matches_brute_force() {
        for (w, h, seed, density) in [(17, 9, 7, 8), (31, 23, 99, 2), (8, 40, 5, 30), (5, 5, 3, 0)] {
            let mut m = mask(w, h, seed, density);
            if density == 0 {
                m[12] = true;
            }
            let got = edt(&m, w, h);
            for j in 0..h {
                for i in 0..w {
                    let mut best = FAR;
                    for jj in 0..h {
                        for ii in 0..w {
                            if m[jj * w + ii] {
                                let d = (i as f32 - ii as f32).powi(2) + (j as f32 - jj as f32).powi(2);
                                best = best.min(d);
                            }
                        }
                    }
                    assert_eq!(got[j * w + i], best, "texel ({i}, {j}) of {w}x{h} seed {seed}");
                }
            }
        }
        // No feature anywhere: every texel stays far.
        assert!(edt(&[false; 12], 4, 3).iter().all(|d| *d >= FAR));
    }

    /// A beach sloping 1:10 down toward +x with the level at 0: the shoreline is at x = 50; the
    /// signed distance crosses zero there within half a texel and grows by one meter per meter.
    #[test]
    fn sloped_beach_puts_the_shoreline_where_the_bed_meets_the_level() {
        let desc = ShoreBakeDesc {
            level: 0.0,
            min: [0.0, -10.0],
            max: [100.0, 10.0],
            cell: 1.0,
        };
        let field = ShoreField::bake(&desc, |x, _| (50.0 - x) * 0.1).expect("bake");
        assert_eq!(field.size, [101, 21]);
        let s = field.sample(70.0, 0.0).expect("inside");
        assert!((s.depth - 2.0).abs() < 1.0e-5, "{s:?}");
        assert!((s.distance - 20.0).abs() < 0.51, "{s:?}");
        let land = field.sample(30.0, 3.0).expect("inside");
        assert!(
            land.depth < 0.0 && (land.distance + 20.0).abs() < 0.51,
            "{land:?}"
        );
        // The zero crossing.
        let mut x = 45.0;
        while field.sample(x, 0.0).expect("inside").distance < 0.0 {
            x += 0.01;
        }
        // The texel at x = 50 has depth 0 and counts as land: the crossing is half a texel out.
        assert!((x - 50.0).abs() <= 0.55, "crossing at {x}");
        assert!(field.sample(-1.0, 0.0).is_none());
        assert!(field.sample(50.0, 10.5).is_none());
        let stats = field.stats();
        assert_eq!(stats.shoreline_texels, 21);
        assert!((stats.wet_fraction - 50.0 / 101.0).abs() < 0.02, "{stats:?}");
    }

    /// A round lake in a bowl: wet inside, dry outside, the distance is the radial gap to the rim.
    #[test]
    fn bowl_lake_is_wet_inside() {
        let desc = ShoreBakeDesc {
            level: 5.0,
            min: [-40.0, -40.0],
            max: [40.0, 40.0],
            cell: 0.5,
        };
        // Bed z = r² / 80: the rim at the level is r = 20.
        let field = ShoreField::bake(&desc, |x, z| (x * x + z * z) / 80.0).expect("bake");
        let center = field.sample(0.0, 0.0).expect("inside");
        assert!((center.depth - 5.0).abs() < 1.0e-4);
        assert!((center.distance - 20.0).abs() < 0.6, "{center:?}");
        let outside = field.sample(30.0, 0.0).expect("inside");
        assert!(
            outside.depth < 0.0 && (outside.distance + 10.0).abs() < 0.6,
            "{outside:?}"
        );
        let diag = field.sample(10.0, 10.0).expect("inside");
        assert!((diag.distance - (20.0 - 200f32.sqrt())).abs() < 0.6, "{diag:?}");
    }

    /// All water or all land: no shoreline, distances say "far"; the cell grows to fit the cap;
    /// bad descriptions are refused.
    #[test]
    fn degenerate_fields_and_caps() {
        let desc = ShoreBakeDesc {
            level: 1.0,
            min: [0.0, 0.0],
            max: [4000.0, 100.0],
            cell: 0.1,
        };
        let field = ShoreField::bake(&desc, |_, _| -3.0).expect("bake");
        assert!(field.size[0] <= MAX_SHORE_SIZE && field.size[1] >= 2);
        assert!(field.cell >= 4000.0 / (MAX_SHORE_SIZE - 1) as f32);
        assert!(field.distance.iter().all(|d| *d > 1000.0));
        let dry = ShoreField::bake(&desc, |_, _| 10.0).expect("bake");
        assert!(dry.distance.iter().all(|d| *d < -1000.0));
        assert_eq!(dry.stats().wet_fraction, 0.0);
        let bad = ShoreBakeDesc {
            max: [0.0, 1.0],
            ..desc
        };
        assert!(matches!(
            ShoreField::bake(&bad, |_, _| 0.0),
            Err(ShoreError::EmptyRect { .. })
        ));
        let nan = ShoreBakeDesc {
            level: f32::NAN,
            ..desc
        };
        assert_eq!(
            ShoreField::bake(&nan, |_, _| 0.0),
            Err(ShoreError::NotFinite("shore.level"))
        );
        // A NaN bed is land, not poison.
        let holes = ShoreField::bake(&desc, |x, _| if x > 2000.0 { f32::NAN } else { -1.0 }).expect("bake");
        assert!(holes.distance.iter().all(|d| d.is_finite()));
    }

    /// Spec 87 § 2.1: the cell is raised until both sides fit, so a span whose `span / 1023`
    /// rounds down in f32 still covers its far corner (clamping the count to 1024 used to cut
    /// it off: `sample(max)` was `None`).
    #[test]
    fn a_raised_cell_still_covers_the_far_edge() {
        // 1000.1781 m: `fl(span / 1023)` is just too small, `ceil(span / cell) + 1` is 1025.
        let span = 1000.1781_f32;
        for max in [[span, span], [span, 10.0], [10.0, span]] {
            let desc = ShoreBakeDesc {
                level: 1.0,
                min: [0.0, 0.0],
                max,
                cell: MIN_SHORE_CELL,
            };
            let field = ShoreField::bake(&desc, |x, _| x * 0.01 - 2.0).expect("bake");
            assert!(field.size[0] <= MAX_SHORE_SIZE && field.size[1] <= MAX_SHORE_SIZE);
            assert!(field.size.contains(&MAX_SHORE_SIZE), "{:?}", field.size);
            assert!(field.cell >= span / (MAX_SHORE_SIZE - 1) as f32);
            for corner in [[0.0, 0.0], [max[0], 0.0], [0.0, max[1]], max] {
                assert!(
                    field.sample(corner[0], corner[1]).is_some(),
                    "corner {corner:?} of {max:?} outside the field: size {:?}, cell {}",
                    field.size,
                    field.cell
                );
            }
        }
    }

    /// Bytes round-trip; another version, bad magic and a truncated payload are refused.
    #[test]
    fn bytes_round_trip_and_versions_are_checked() {
        let desc = ShoreBakeDesc {
            level: 0.0,
            min: [0.0, 0.0],
            max: [20.0, 10.0],
            cell: 1.0,
        };
        let field = ShoreField::bake(&desc, |x, _| 10.0 - x).expect("bake");
        let bytes = field.to_bytes();
        assert_eq!(&bytes[..4], b"RGSH");
        assert_eq!(ShoreField::from_bytes(&bytes).expect("read"), field);
        // The same bake gives the same bytes.
        assert_eq!(
            ShoreField::bake(&desc, |x, _| 10.0 - x).expect("bake").to_bytes(),
            bytes
        );
        let mut other = bytes.clone();
        other[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            ShoreField::from_bytes(&other),
            Err(ShoreError::Version {
                found: 2,
                expected: 1
            })
        );
        assert_eq!(
            ShoreField::from_bytes(b"NOPE\x01\0\0\0"),
            Err(ShoreError::BadMagic)
        );
        assert!(matches!(
            ShoreField::from_bytes(&bytes[..bytes.len() / 2]),
            Err(ShoreError::Corrupt(_))
        ));
    }

    /// The terrain bake samples the map's bilinear height.
    #[test]
    fn terrain_bake_follows_the_map() {
        // 3 × 3 samples over 20 m: the middle row is high.
        let heights = vec![-2.0, -2.0, -2.0, 3.0, 3.0, 3.0, -2.0, -2.0, -2.0];
        let map = TerrainMap::from_heights(3, 3, heights, 20.0, 1.0, 0.0).expect("map");
        let field = ShoreField::bake_terrain(&map, 0.0, [-10.0, -10.0], [10.0, 10.0]).expect("bake");
        assert_eq!(field.cell, 10.0);
        let mid = field.sample(0.0, 0.0).expect("inside");
        let edge = field.sample(0.0, -10.0).expect("inside");
        assert!(mid.depth < 0.0 && edge.depth > 0.0, "{mid:?} {edge:?}");
        assert!((mid.depth - (0.0 - map.sample_bilinear(0.0, 0.0))).abs() < 1.0e-5);
    }

    /// Spec 87 § 2.1: `bake_terrain` uses `max(map spacing, MIN_SHORE_CELL)`: a map sampled every
    /// 0.1 m still bakes at 0.25 m.
    #[test]
    fn terrain_bake_never_goes_below_the_minimum_cell() {
        // 11 × 11 samples over 1 m: spacing 0.1 m, a ridge along x = 0.
        let heights = (0..121).map(|i| if i % 11 == 5 { 1.0 } else { -1.0 }).collect();
        let map = TerrainMap::from_heights(11, 11, heights, 1.0, 1.0, 0.0).expect("map");
        let field = ShoreField::bake_terrain(&map, 0.0, [-0.5, -0.5], [0.5, 0.5]).expect("bake");
        assert_eq!(field.cell, MIN_SHORE_CELL);
        assert_eq!(field.size, [5, 5], "1 m at 0.25 m: 4 cells, 5 texels");
        assert!(field.sample(0.5, 0.5).is_some(), "the far corner is inside");
    }

    /// Shoaling: 1 in deep water, dips below 1 at intermediate depth and grows in the shallows,
    /// continuous at the deep-water cut; the scale never lets a wave exceed the breaking cap, is
    /// 0 on land and at most SHOAL_MAX.
    #[test]
    fn shoaling_and_breaking() {
        let k = std::f32::consts::TAU / 40.0;
        assert_eq!(shoal_coefficient(k, 100.0), 1.0);
        // Continuous at the deep-water cut (no visible step).
        let below = shoal_coefficient(k, DEEP_KH / k * 0.9999);
        assert!((below - 1.0).abs() < 1.0e-4, "{below}");
        let mid = shoal_coefficient(k, 1.2 / k);
        assert!(mid < 1.0 && mid > 0.9, "{mid}");
        assert!(shoal_coefficient(k, 0.5) > 1.2);
        let mut last = f32::INFINITY;
        for i in 1..400 {
            let h = i as f32 * 0.05;
            let ks = shoal_coefficient(k, h);
            assert!(ks.is_finite() && ks > 0.0);
            if k * h < 1.0 {
                assert!(ks < last, "falls with depth in the shallows");
            }
            last = ks;
            for a in [0.1f32, 0.5, 1.5, 4.0] {
                let (s, ratio) = shoal_scale(k, a, h);
                assert!(
                    s * a <= 0.5 * BREAKING_INDEX * h * (1.0 + 1.0e-5),
                    "h {h} a {a} s {s}"
                );
                assert!((0.0..=SHOAL_MAX).contains(&s));
                assert!(ratio >= 0.0);
            }
        }
        assert_eq!(shoal_scale(k, 1.0, 0.0).0, 0.0);
        assert_eq!(shoal_scale(k, 1.0, -3.0).0, 0.0);
        assert_eq!(shoal_scale(k, 1.0, f32::NAN).0, 0.0);
        assert_eq!(breaker_foam(0.5), 0.0);
        assert_eq!(breaker_foam(1.2), 1.0);
        assert_eq!(shore_foam(0.0, 3.0), 1.0);
        assert_eq!(shore_foam(3.0, 3.0), 0.0);
        assert_eq!(shore_foam(-1.0, 3.0), 0.0);
        assert_eq!(shore_foam(1.0, 0.0), 0.0);
    }

    /// A field with arrays longer than `size` is consistent; its stats count its own texels.
    #[test]
    fn stats_ignore_samples_past_the_field() {
        let long = ShoreField {
            format_version: SHORE_FORMAT_VERSION,
            origin: [0.0; 2],
            cell: 1.0,
            size: [2, 2],
            level: 0.0,
            distance: vec![0.0; 5],
            depth: vec![1.0, 1.0, 1.0, 1.0, 9.0],
        };
        assert!(long.is_consistent());
        let stats = long.stats();
        assert_eq!(stats.wet_fraction, 1.0);
        assert_eq!(stats.max_depth, 1.0);
    }

    #[test]
    fn inconsistent_fields_do_not_abort() {
        let bad = ShoreField {
            format_version: SHORE_FORMAT_VERSION,
            origin: [0.0; 2],
            cell: 1.0,
            size: [2, 2],
            level: 0.0,
            distance: Vec::new(),
            depth: Vec::new(),
        };
        assert!(!bad.is_consistent());
        let stats = bad.stats();
        assert_eq!(stats.shoreline_texels, 0);
        assert_eq!(stats.wet_fraction, 0.0);
        let zero = ShoreField {
            size: [0, 2],
            depth: vec![0.0],
            distance: vec![0.0],
            ..bad.clone()
        };
        assert!(!zero.is_consistent());
        assert!(zero.sample(0.0, 0.0).is_none());
        assert!(edt(&[true], 2, 2).is_empty());
        assert!(edt(&[], 0, 0).is_empty());
    }
}
