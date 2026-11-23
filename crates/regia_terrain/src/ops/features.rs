// SPDX-License-Identifier: MPL-2.0
//! Shaping operators: terraces, impact craters, flattened build pads and domain warp.
//!
//! * **Terrace** quantizes height into treads `step_height_m` apart, with a smoothstep riser whose
//!   width shrinks as `edge_sharpness` grows; low-frequency noise shifts where each riser falls
//!   (`width_jitter`) and a noise mask lets the original slope show through (`collapse`).
//! * **Crater** adds a parabolic bowl from `-depth_m` at the center to `+rim_height_m` at the rim
//!   crest, an ejecta blanket decaying as `r^-ejecta_falloff` to zero at `ejecta_extent` radii, and an
//!   optional Gaussian central peak: the simple-crater shape of Melosh, "Impact Cratering: A Geologic
//!   Process" (1989), ch. 8, with the power-law ejecta thickness of McGetchin, Settle & Head (1973).
//! * **Flatten pad** sets every sample inside a circle or a rotated rectangle (its signed distance
//!   field, Quilez's box SDF) to one height and blends outside it with a smoothstep over
//!   `feather_m` meters.
//! * **Domain warp** resamples the grid bilinearly at positions displaced by two fBm fields
//!   (Quilez, "Domain warping", 2002): heights keep their values and only move.

use serde::{Deserialize, Serialize};

use super::noise::{Field, NoiseParams};
use super::{GridFrame, OpError, require, sin_cos_deg, smoothstep};

/// Settings of [`terrace`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct TerraceParams {
    /// Seed of the jitter and collapse noise.
    pub seed: u32,
    /// Vertical spacing of the treads, meters (> 0).
    pub step_height_m: f64,
    /// Treads sit at `base_m + k * step_height_m`.
    pub base_m: f64,
    /// 0 to 1: how far, as a fraction of `step_height_m`, noise shifts where each riser falls, so
    /// tread widths vary across the map. Treads stay at their levels.
    pub width_jitter: f64,
    /// Wavelength of the jitter noise, meters.
    pub jitter_wavelength_m: f64,
    /// 0 to 1: 0 leaves the ground unchanged, 1 gives near-vertical risers and exactly flat treads.
    pub edge_sharpness: f64,
    /// 0 to 1: how much of the area (in soft-edged patches) slumps back to the original slope; 1
    /// restores the input everywhere.
    pub collapse: f64,
    /// Wavelength of the collapse patches, meters.
    pub collapse_wavelength_m: f64,
}

impl Default for TerraceParams {
    fn default() -> Self {
        Self {
            seed: 0,
            step_height_m: 6.0,
            base_m: 0.0,
            width_jitter: 0.3,
            jitter_wavelength_m: 150.0,
            edge_sharpness: 0.8,
            collapse: 0.0,
            collapse_wavelength_m: 120.0,
        }
    }
}

impl TerraceParams {
    /// Treads `step_height_m` apart with the other settings at their defaults.
    #[must_use]
    pub fn new(step_height_m: f64) -> Self {
        Self {
            step_height_m,
            ..Self::default()
        }
    }

    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// [`OpError::BadParam`] naming the first bad setting.
    pub fn validate(&self) -> Result<(), OpError> {
        let unit = |v: f64| v.is_finite() && (0.0..=1.0).contains(&v);
        require(
            self.step_height_m.is_finite() && self.step_height_m > 0.0,
            "step_height_m",
            "must be finite and positive",
        )?;
        require(self.base_m.is_finite(), "base_m", "must be finite")?;
        require(unit(self.width_jitter), "width_jitter", "must be 0 to 1")?;
        require(unit(self.edge_sharpness), "edge_sharpness", "must be 0 to 1")?;
        require(unit(self.collapse), "collapse", "must be 0 to 1")?;
        require(
            self.jitter_wavelength_m.is_finite() && self.jitter_wavelength_m > 0.0,
            "jitter_wavelength_m",
            "must be finite and positive",
        )?;
        require(
            self.collapse_wavelength_m.is_finite() && self.collapse_wavelength_m > 0.0,
            "collapse_wavelength_m",
            "must be finite and positive",
        )
    }
}

/// Width of the collapse mask's soft edge, in mask units.
const COLLAPSE_SOFT: f64 = 0.1;

/// Cuts the ground into flat treads and risers, like weathered strata.
///
/// With `edge_sharpness` 1 and no jitter, treads sit exactly at `base_m + k * step_height_m`; every
/// output lies within `step_height_m * (1 + width_jitter)` of the input. The collapse mask is a
/// pointwise function of world position (fBm mapped to `[0, 1]`), so it is the same in every window
/// of a map; `collapse` is the approximate share of the area restored.
///
/// # Errors
///
/// A frame or height error, or [`OpError::BadParam`].
pub fn terrace(frame: &GridFrame, heights: &[f32], p: &TerraceParams) -> Result<Vec<f32>, OpError> {
    frame.check_heights(heights)?;
    p.validate()?;
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.op.terrace").entered();
    let jitter = Field::new(&NoiseParams::new(
        p.seed.wrapping_add(7),
        1.0,
        p.jitter_wavelength_m,
        3,
    ))?;
    let mask = Field::new(&NoiseParams::new(
        p.seed.wrapping_add(31),
        1.0,
        p.collapse_wavelength_m,
        3,
    ))?;
    let s = p.edge_sharpness;
    let riser = 1.0 - 0.95 * s;
    let mut out = Vec::with_capacity(heights.len());
    for r in 0..frame.rows {
        for c in 0..frame.cols {
            let h = f64::from(heights[r as usize * frame.cols as usize + c as usize]);
            if s == 0.0 || p.collapse == 1.0 {
                out.push(h as f32);
                continue;
            }
            let [x, z] = frame.position(c, r);
            let shift = if p.width_jitter > 0.0 {
                jitter.fbm(x, z).clamp(-1.0, 1.0) * p.width_jitter * p.step_height_m
            } else {
                0.0
            };
            let t = (h - p.base_m + shift) / p.step_height_m;
            let k = t.floor();
            let f = t - k;
            let stepped = smoothstep((f - 0.5) / riser + 0.5);
            let terraced = p.base_m + (k + stepped) * p.step_height_m;
            let mut v = h * (1.0 - s) + terraced * s;
            if p.collapse > 0.0 {
                let u = (mask.fbm(x, z) + 1.0) * 0.5;
                let w = smoothstep((p.collapse * (1.0 + COLLAPSE_SOFT) - u) / COLLAPSE_SOFT);
                v = v * (1.0 - w) + h * w;
            }
            out.push(v as f32);
        }
    }
    Ok(out)
}

/// Settings of [`crater`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct CraterParams {
    /// World `(x, z)` of the center, meters.
    pub center: [f64; 2],
    /// Rim-crest radius, meters (> 0).
    pub radius_m: f64,
    /// Floor depth below the surrounding ground at the center, meters.
    pub depth_m: f64,
    /// Rim crest height above the surrounding ground, meters.
    pub rim_height_m: f64,
    /// Decay exponent of the ejecta blanket (higher drops off faster).
    pub ejecta_falloff: f64,
    /// The blanket reaches this many radii from the center (> 1).
    pub ejecta_extent: f64,
    /// Height of an optional central peak above the bowl floor, meters.
    pub central_peak_m: f64,
    /// The peak's Gaussian width as a fraction of `radius_m`.
    pub central_peak_radius_frac: f64,
}

impl Default for CraterParams {
    fn default() -> Self {
        Self {
            center: [0.0, 0.0],
            radius_m: 50.0,
            depth_m: 8.0,
            rim_height_m: 2.0,
            ejecta_falloff: 2.5,
            ejecta_extent: 2.5,
            central_peak_m: 0.0,
            central_peak_radius_frac: 0.12,
        }
    }
}

impl CraterParams {
    /// A crater at `center` with `radius_m`, `depth_m` and `rim_height_m`, other settings default.
    #[must_use]
    pub fn new(center: [f64; 2], radius_m: f64, depth_m: f64, rim_height_m: f64) -> Self {
        Self {
            center,
            radius_m,
            depth_m,
            rim_height_m,
            ..Self::default()
        }
    }

    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// [`OpError::BadParam`] naming the first bad setting.
    pub fn validate(&self) -> Result<(), OpError> {
        require(
            self.center.iter().all(|v| v.is_finite()),
            "center",
            "must be finite",
        )?;
        require(
            self.radius_m.is_finite() && self.radius_m > 0.0,
            "radius_m",
            "must be finite and positive",
        )?;
        require(self.depth_m.is_finite(), "depth_m", "must be finite")?;
        require(self.rim_height_m.is_finite(), "rim_height_m", "must be finite")?;
        require(
            self.ejecta_falloff.is_finite(),
            "ejecta_falloff",
            "must be finite",
        )?;
        require(
            self.ejecta_extent.is_finite() && self.ejecta_extent > 1.0,
            "ejecta_extent",
            "must be finite and greater than 1",
        )?;
        require(
            self.central_peak_m.is_finite(),
            "central_peak_m",
            "must be finite",
        )?;
        require(
            self.central_peak_radius_frac.is_finite() && self.central_peak_radius_frac > 0.0,
            "central_peak_radius_frac",
            "must be finite and positive",
        )
    }

    /// How far from the center the crater changes the ground, meters.
    #[must_use]
    pub fn reach_m(&self) -> f64 {
        self.radius_m * self.ejecta_extent
    }

    /// The height the crater adds at `r = distance / radius_m` from its center, meters.
    #[must_use]
    pub fn delta_at(&self, r: f64) -> f64 {
        let det_pow = |b: f64, e: f64| f64::from(regia_core::math::detmath::det_powf(b as f32, e as f32));
        let mut delta = if r <= 1.0 {
            -self.depth_m + (self.depth_m + self.rim_height_m) * r * r
        } else {
            let edge = det_pow(self.ejecta_extent, -self.ejecta_falloff);
            let blanket = self.rim_height_m * (det_pow(r, -self.ejecta_falloff) - edge) / (1.0 - edge);
            blanket.max(0.0)
        };
        if self.central_peak_m != 0.0 && r <= 1.0 {
            let q = r / self.central_peak_radius_frac.max(1e-3);
            // e^(−q²) = 2^(−q² · log2 e)
            delta += self.central_peak_m * det_pow(2.0, -q * q * std::f64::consts::LOG2_E);
        }
        delta
    }
}

/// Adds an impact crater: bowl, raised rim, ejecta blanket and an optional central peak. The crater
/// is added to the ground, so it follows a slope; samples beyond [`CraterParams::reach_m`] keep their
/// bits.
///
/// # Errors
///
/// A frame or height error, or [`OpError::BadParam`].
pub fn crater(frame: &GridFrame, heights: &[f32], p: &CraterParams) -> Result<Vec<f32>, OpError> {
    frame.check_heights(heights)?;
    p.validate()?;
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.op.crater").entered();
    let mut out = heights.to_vec();
    let reach = if p.rim_height_m > 0.0 {
        p.reach_m()
    } else {
        p.radius_m
    };
    let Some([c0, r0, c1, r1]) = frame.window(p.center[0], p.center[1], reach) else {
        return Ok(out);
    };
    for r in r0..=r1 {
        for c in c0..=c1 {
            let [x, z] = frame.position(c, r);
            let (dx, dz) = (x - p.center[0], z - p.center[1]);
            let rr = (dx * dx + dz * dz).sqrt() / p.radius_m;
            if rr > p.ejecta_extent {
                continue;
            }
            let i = r as usize * frame.cols as usize + c as usize;
            out[i] = (f64::from(heights[i]) + p.delta_at(rr)) as f32;
        }
    }
    Ok(out)
}

/// The footprint of a flattened pad (JSON: `{"circle": {"radius_m": 20}}` or
/// `{"rect": {"size_m": [40, 20], "rotation_deg": 0}}`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PadShape {
    /// A circle.
    Circle {
        /// Radius of the fully flat area, meters.
        radius_m: f64,
    },
    /// A rectangle.
    Rect {
        /// Full `(length along, width across)` of the flat area, meters; "along" is local +X.
        size_m: [f64; 2],
        /// Rotation of local +X, degrees from +X toward +Z.
        rotation_deg: f64,
    },
}

/// Settings of [`flatten_pad`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct PadParams {
    /// World `(x, z)` of the pad center, meters.
    pub center: [f64; 2],
    /// The flat footprint.
    pub shape: PadShape,
    /// The pad height, meters; `None` takes the mean of the input heights inside the pad.
    pub height_m: Option<f64>,
    /// Width of the smooth blend outside the footprint, meters (0 = a hard edge).
    pub feather_m: f64,
}

impl Default for PadParams {
    fn default() -> Self {
        Self {
            center: [0.0, 0.0],
            shape: PadShape::Circle { radius_m: 20.0 },
            height_m: None,
            feather_m: 20.0,
        }
    }
}

impl PadParams {
    /// A circular pad of `radius_m` at `center`, other settings default.
    #[must_use]
    pub fn circle(center: [f64; 2], radius_m: f64) -> Self {
        Self {
            center,
            shape: PadShape::Circle { radius_m },
            ..Self::default()
        }
    }

    /// A rectangular pad of full size `size_m` (along, across) turned `rotation_deg`.
    #[must_use]
    pub fn rect(center: [f64; 2], size_m: [f64; 2], rotation_deg: f64) -> Self {
        Self {
            center,
            shape: PadShape::Rect { size_m, rotation_deg },
            ..Self::default()
        }
    }

    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// [`OpError::BadParam`] naming the first bad setting.
    pub fn validate(&self) -> Result<(), OpError> {
        require(
            self.center.iter().all(|v| v.is_finite()),
            "center",
            "must be finite",
        )?;
        match self.shape {
            PadShape::Circle { radius_m } => require(
                radius_m.is_finite() && radius_m > 0.0,
                "radius_m",
                "must be finite and positive",
            )?,
            PadShape::Rect { size_m, rotation_deg } => {
                require(
                    size_m.iter().all(|v| v.is_finite() && *v > 0.0),
                    "size_m",
                    "must be finite and positive",
                )?;
                require(rotation_deg.is_finite(), "rotation_deg", "must be finite")?;
            }
        }
        require(
            self.height_m.is_none_or(f64::is_finite),
            "height_m",
            "must be finite",
        )?;
        require(
            self.feather_m.is_finite() && self.feather_m >= 0.0,
            "feather_m",
            "must be finite and at least 0",
        )
    }

    /// How far from the center the pad changes the ground, meters.
    #[must_use]
    pub fn reach_m(&self) -> f64 {
        let r = match self.shape {
            PadShape::Circle { radius_m } => radius_m,
            PadShape::Rect { size_m, .. } => 0.5 * (size_m[0] * size_m[0] + size_m[1] * size_m[1]).sqrt(),
        };
        r + self.feather_m
    }
}

/// Levels a pad (a build pocket) and blends it into the ground around it.
///
/// Every sample inside the footprint (signed distance ≤ 0) equals the pad height exactly; outside
/// it the pad's weight falls from 1 to 0 with a smoothstep over `feather_m` meters of distance from
/// the edge; samples beyond [`PadParams::reach_m`] keep their bits.
///
/// # Errors
///
/// A frame or height error, [`OpError::BadParam`], or [`OpError::PadEmpty`] when the pad has no
/// height and covers no sample.
pub fn flatten_pad(frame: &GridFrame, heights: &[f32], p: &PadParams) -> Result<Vec<f32>, OpError> {
    frame.check_heights(heights)?;
    p.validate()?;
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.op.flatten_pad").entered();
    let (sin, cos) = match p.shape {
        PadShape::Rect { rotation_deg, .. } => sin_cos_deg(rotation_deg),
        PadShape::Circle { .. } => (0.0, 1.0),
    };
    let sdf = |x: f64, z: f64| -> f64 {
        let (dx, dz) = (x - p.center[0], z - p.center[1]);
        match p.shape {
            PadShape::Circle { radius_m } => (dx * dx + dz * dz).sqrt() - radius_m,
            PadShape::Rect { size_m, .. } => {
                let u = dx * cos + dz * sin;
                let v = -dx * sin + dz * cos;
                let qu = u.abs() - size_m[0] * 0.5;
                let qv = v.abs() - size_m[1] * 0.5;
                let (ou, ov) = (qu.max(0.0), qv.max(0.0));
                (ou * ou + ov * ov).sqrt() + qu.max(qv).min(0.0)
            }
        }
    };
    let mut out = heights.to_vec();
    let Some([c0, r0, c1, r1]) = frame.window(p.center[0], p.center[1], p.reach_m()) else {
        return if p.height_m.is_some() {
            Ok(out)
        } else {
            Err(OpError::PadEmpty)
        };
    };
    let cols = frame.cols as usize;
    let target = match p.height_m {
        Some(h) => h,
        None => {
            let (mut sum, mut n) = (0.0, 0u64);
            for r in r0..=r1 {
                for c in c0..=c1 {
                    let [x, z] = frame.position(c, r);
                    if sdf(x, z) <= 0.0 {
                        sum += f64::from(heights[r as usize * cols + c as usize]);
                        n += 1;
                    }
                }
            }
            if n == 0 {
                return Err(OpError::PadEmpty);
            }
            sum / n as f64
        }
    };
    let flat = target as f32;
    for r in r0..=r1 {
        for c in c0..=c1 {
            let [x, z] = frame.position(c, r);
            let d = sdf(x, z);
            let i = r as usize * cols + c as usize;
            if d <= 0.0 {
                out[i] = flat;
            } else if d < p.feather_m {
                let w = 1.0 - smoothstep(d / p.feather_m);
                let h = f64::from(heights[i]);
                out[i] = (h + (target - h) * w) as f32;
            }
        }
    }
    Ok(out)
}

/// Settings of [`domain_warp`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct WarpParams {
    /// Seed of the two displacement fields.
    pub seed: u32,
    /// Largest horizontal displacement, meters (0 returns the input unchanged).
    pub strength_m: f64,
    /// Wavelength of the displacement fields, meters.
    pub wavelength_m: f64,
    /// Octaves of the displacement fields, 1 to 16.
    pub octaves: u32,
}

impl Default for WarpParams {
    fn default() -> Self {
        Self {
            seed: 0,
            strength_m: 40.0,
            wavelength_m: 300.0,
            octaves: 3,
        }
    }
}

impl WarpParams {
    /// A warp of `strength_m` over `wavelength_m`, other settings default.
    #[must_use]
    pub fn new(seed: u32, strength_m: f64, wavelength_m: f64) -> Self {
        Self {
            seed,
            strength_m,
            wavelength_m,
            ..Self::default()
        }
    }

    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// [`OpError::BadParam`] naming the first bad setting.
    pub fn validate(&self) -> Result<(), OpError> {
        require(
            self.strength_m.is_finite() && self.strength_m >= 0.0,
            "strength_m",
            "must be finite and at least 0",
        )?;
        NoiseParams::new(0, 1.0, self.wavelength_m, self.octaves).validate()
    }
}

/// Warps the ground by resampling it at noise-displaced positions.
///
/// Sample `(c, r)` reads the input bilinearly at `(c + dx · s / cell_x, r + dz · s / cell_z)`, where
/// `dx`, `dz` are fBm fields in `[-1, 1]` at the sample's world position and `s` is `strength_m`.
/// Reads past the grid edge clamp to it, so every output lies within the input's range. The
/// displacement depends only on world position: a window of a map warps like the map wherever its
/// reads stay inside the window (give a window `strength_m` of margin).
///
/// # Errors
///
/// A frame or height error, or [`OpError::BadParam`].
pub fn domain_warp(frame: &GridFrame, heights: &[f32], p: &WarpParams) -> Result<Vec<f32>, OpError> {
    frame.check_heights(heights)?;
    p.validate()?;
    let _span = tracing::info_span!(target: "regia_terrain", "terrain.op.domain_warp").entered();
    if p.strength_m == 0.0 {
        return Ok(heights.to_vec());
    }
    let base = p.seed.wrapping_mul(2);
    let warp = |seed: u32| Field::new(&NoiseParams::new(seed, 1.0, p.wavelength_m, p.octaves));
    let fx = warp(base.wrapping_add(11))?;
    let fz = warp(base.wrapping_add(12))?;
    let (cols, rows) = (frame.cols as usize, frame.rows as usize);
    let (max_c, max_r) = ((cols - 1) as f64, (rows - 1) as f64);
    let at = |c: usize, r: usize| f64::from(heights[r * cols + c]);
    let mut out = Vec::with_capacity(heights.len());
    for r in 0..frame.rows {
        for c in 0..frame.cols {
            let [x, z] = frame.position(c, r);
            let sc = (f64::from(c) + fx.fbm(x, z) * p.strength_m / frame.cell_m[0]).clamp(0.0, max_c);
            let sr = (f64::from(r) + fz.fbm(x, z) * p.strength_m / frame.cell_m[1]).clamp(0.0, max_r);
            let c0 = (sc.floor() as usize).min(cols - 2);
            let r0 = (sr.floor() as usize).min(rows - 2);
            let (fc, fr) = (sc - c0 as f64, sr - r0 as f64);
            let top = at(c0, r0) * (1.0 - fc) + at(c0 + 1, r0) * fc;
            let bot = at(c0, r0 + 1) * (1.0 - fc) + at(c0 + 1, r0 + 1) * fc;
            out.push((top * (1.0 - fr) + bot * fr) as f32);
        }
    }
    Ok(out)
}
