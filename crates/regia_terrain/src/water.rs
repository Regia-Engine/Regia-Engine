// SPDX-License-Identifier: MPL-2.0
//! The shoreline water plane (spec 26 § 1): a planar surface at a fixed level with
//! depth-based color, shoreline foam, ripples and sun specular. No simulation.
//!
//! The GPU pass samples the terrain height texture to compute water depth at each fragment;
//! time-of-day input (sun direction/color) comes from the frame uniform.
//!
//! # Shading model (ADR-1237)
//!
//! The functions below are the CPU reference of `gpu/water.wgsl`; the shader mirrors them
//! term for term and the unit tests pin both (constants are compared by text).
//!
//! - **Reflection**: Schlick's Fresnel with the air–water normal-incidence reflectance
//!   [`WATER_F0`] against the frame's sky radiance (the uniform sky the terrain is lit by:
//!   the probe fallback of spec 26). Seen from below, the interface reflects totally past the
//!   critical angle ([`interface_reflectance`]).
//! - **Transmission**: the view ray refracts into the water by Snell's law; the light it
//!   carries back crosses `depth / cos θt` meters of water ([`optical_path`]). The water body's
//!   opacity follows Beer–Lambert over that path ([`WaterPlane::body_opacity`]), so shallow
//!   water shows the bed and deep water its own color.
//! - **Shoreline**: the opacity fades in over [`SHORE_FADE`] meters of depth from the
//!   [`SHORE_CUTOFF`] where the plane stops being drawn, so the waterline has no step
//!   ([`shore_fade`]). The foam band is measured horizontally from the shoreline
//!   ([`WaterPlane::foam`], [`shore_distance`]), as `foam_width` documents.
//! - **Ripples**: the drawn grid fades out each ripple it cannot resolve (fewer than two
//!   vertices per half wavelength, [`vertex_wave_fade`]); the shading normal keeps both.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sampler::TerrainMap;

/// Refractive index of water for visible light (fresh water near 20 °C).
pub const WATER_IOR: f32 = 1.333;

/// Reflectance of the air–water interface at normal incidence, `((n − 1) / (n + 1))²` for
/// [`WATER_IOR`].
pub const WATER_F0: f32 = 0.020_373;

/// Depth (meters) below which the plane is not drawn: `depth < -SHORE_CUTOFF` is dry land.
/// The small negative margin hides the gap between the height texture and the drawn terrain.
pub const SHORE_CUTOFF: f32 = 0.05;

/// Depth (meters, from `-SHORE_CUTOFF`) over which the water's opacity fades in at the
/// waterline.
pub const SHORE_FADE: f32 = 0.2;

/// Beer–Lambert optical depth at `max_depth` of path: the water body reaches
/// `1 − e⁻³ ≈ 95 %` of its `alpha` at half `max_depth` (where its color is halfway to
/// `deep_color`).
pub const ABSORPTION_AT_MAX_DEPTH: f32 = 6.0;

/// Coverage of the foam at the waterline (the rest of the surface shows through).
pub const FOAM_COVERAGE: f32 = 0.8;

/// Smallest bed slope (rise over run) used to turn a depth into a distance from the shore;
/// a flat bed would put the shoreline at infinity.
pub const MIN_SHORE_SLOPE: f32 = 0.02;

/// Grid cells of the drawn water plane along each axis (`gpu/water.wgsl` `GRID`).
pub const WATER_GRID: u32 = 64;

/// Wave vectors of the two ripples per unit `wave_frequency` (`(x, z)`); the second is
/// `1.3 × (−0.6, 1)`.
const WAVE_DIRS: [[f32; 2]; 2] = [[0.7, 0.3], [-0.78, 1.3]];
/// Angular speed of the two ripples, radians per second.
const WAVE_SPEED: [f32; 2] = [1.1, 0.9];
/// Amplitude of the two ripples relative to `wave_amplitude`.
const WAVE_GAIN: [f32; 2] = [1.0, 0.6];

/// Hermite `smoothstep`, clamped (WGSL's built-in).
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Schlick's approximation of the Fresnel reflectance of the air–water interface for a ray at
/// `cos_theta` to the normal (clamped to `0..=1`).
#[must_use]
pub fn fresnel_schlick(cos_theta: f32) -> f32 {
    let c = 1.0 - cos_theta.clamp(0.0, 1.0);
    let c2 = c * c;
    WATER_F0 + (1.0 - WATER_F0) * c2 * c2 * c
}

/// Cosine of the refracted angle of a ray entering water from air at `cos_incident` to the
/// normal (Snell's law). Always defined: the angle never exceeds the critical angle, so the
/// result is at least `sqrt(1 − 1/n²) ≈ 0.661`.
#[must_use]
pub fn refracted_cos(cos_incident: f32) -> f32 {
    let c = cos_incident.clamp(0.0, 1.0);
    let sin2_t = (1.0 - c * c) / (WATER_IOR * WATER_IOR);
    (1.0 - sin2_t).max(0.0).sqrt()
}

/// Reflectance of the interface for a view at `cos_view` = N·V: positive when the eye is
/// above the surface, negative when it is below (looking up from inside the water). From
/// below, a ray past the critical angle (≈ 48.6°) reflects totally (1); inside the window the
/// reflectance is Schlick's at the air-side angle.
#[must_use]
pub fn interface_reflectance(cos_view: f32) -> f32 {
    if cos_view >= 0.0 {
        return fresnel_schlick(cos_view);
    }
    let c = (-cos_view).min(1.0);
    let sin2_t = WATER_IOR * WATER_IOR * (1.0 - c * c);
    if sin2_t >= 1.0 {
        1.0
    } else {
        fresnel_schlick((1.0 - sin2_t).sqrt())
    }
}

/// Length of water (meters) the light seen through the surface crosses: down to a bed at
/// vertical `depth` along the view ray refracted at `cos_view` = N·V, i.e.
/// `depth / cos θt`. Zero for `depth <= 0`.
#[must_use]
pub fn optical_path(depth: f32, cos_view: f32) -> f32 {
    depth.max(0.0) / refracted_cos(cos_view)
}

/// Opacity multiplier at the waterline: 0 at `depth = -SHORE_CUTOFF` (where the plane stops
/// being drawn), 1 from `SHORE_FADE − SHORE_CUTOFF` meters of depth on. Continuous, so the
/// waterline shows no step.
#[must_use]
pub fn shore_fade(depth: f32) -> f32 {
    smoothstep(-SHORE_CUTOFF, SHORE_FADE - SHORE_CUTOFF, depth)
}

/// Horizontal distance (meters) from a point under `depth` meters of water to the shoreline,
/// for a locally planar bed rising at `slope` (rise over run, at least [`MIN_SHORE_SLOPE`]).
#[must_use]
pub fn shore_distance(depth: f32, slope: f32) -> f32 {
    let slope = if slope.is_finite() {
        slope.max(MIN_SHORE_SLOPE)
    } else {
        MIN_SHORE_SLOPE
    };
    depth.max(0.0) / slope
}

/// Displacement kept by the drawn grid for a ripple of `wave_number` (radians per meter) on
/// cells of `cell_size` meters: 1 with at least four vertices per wavelength, 0 at the
/// Nyquist limit (two) and beyond, linear in between. Unresolved ripples would otherwise
/// alias into a coarse, wrong wobble of the whole plane.
#[must_use]
pub fn vertex_wave_fade(cell_size: f32, wave_number: f32) -> f32 {
    let phase_per_cell = cell_size.max(0.0) * wave_number.abs();
    (2.0 - 2.0 * phase_per_cell / std::f32::consts::PI).clamp(0.0, 1.0)
}

/// WGSL's `refract`: the direction of `incident` (unit) refracted at a surface of unit
/// `normal` (facing against `incident`) with index ratio `eta`; zero on total internal
/// reflection.
fn refract(incident: [f32; 3], normal: [f32; 3], eta: f32) -> [f32; 3] {
    let n_dot_i = normal[0] * incident[0] + normal[1] * incident[1] + normal[2] * incident[2];
    let k = 1.0 - eta * eta * (1.0 - n_dot_i * n_dot_i);
    if k < 0.0 {
        return [0.0; 3];
    }
    let s = eta * n_dot_i + k.sqrt();
    std::array::from_fn(|i| eta * incident[i] - s * normal[i])
}

/// Where the view ray refracted into the water at `point` meets a bed `depth` meters below
/// it (taken as locally flat): Snell's law about the unit `normal`, with `view_dir` the unit
/// direction from the point to the eye (above the surface). A normal turned away from the
/// eye (a ripple at a grazing view) refracts about the still plane's `+Y` instead.
/// Screen-space refraction samples the frame where this point projects (`water.wgsl`
/// `refracted_bed_point`).
#[must_use]
pub fn refracted_bed_point(point: [f32; 3], view_dir: [f32; 3], normal: [f32; 3], depth: f32) -> [f32; 3] {
    let facing = if normal[0] * view_dir[0] + normal[1] * view_dir[1] + normal[2] * view_dir[2] > 0.0 {
        normal
    } else {
        [0.0, 1.0, 0.0]
    };
    let t = refract(view_dir.map(|v| -v), facing, 1.0 / WATER_IOR);
    let length = depth.max(0.0) / (-t[1]).max(0.05);
    std::array::from_fn(|i| point[i] + t[i] * length)
}

/// What a point of the water plane looks like, from [`WaterPlane::shade`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterShade {
    /// Surface color in the frame's scene-linear units (before fog and the display
    /// transform), un-premultiplied: what covers [`Self::alpha`] of the pixel.
    pub color: [f32; 3],
    /// Coverage over what lies behind the surface (straight alpha).
    pub alpha: f32,
    /// Interface reflectance used (Fresnel, or total internal reflection from below).
    pub reflectance: f32,
    /// Opacity of the water body along the refracted path.
    pub body_opacity: f32,
    /// Foam coverage (`0..=FOAM_COVERAGE`).
    pub foam: f32,
}

/// The inputs of [`WaterPlane::shade`] at one point of the surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterShadeInput {
    /// Vertical water depth over the bed, meters (negative above the waterline).
    pub depth: f32,
    /// Horizontal distance to the shoreline, meters ([`shore_distance`]).
    pub shore_distance: f32,
    /// N·V: cosine between the surface normal and the direction to the eye (negative when the
    /// eye is under the surface).
    pub cos_view: f32,
    /// Distance from the eye to the point, meters (absorption between an underwater eye and
    /// the surface).
    pub eye_distance: f32,
    /// Foam mottle noise in `0..=1`.
    pub mottle: f32,
    /// Lighting of the water body and foam per unit albedo (sun · N·L / π + sky), scene-linear.
    pub irradiance: [f32; 3],
    /// Sky radiance reflected by the surface, scene-linear.
    pub sky: [f32; 3],
    /// Sun glint radiance added on top, scene-linear.
    pub glint: [f32; 3],
}

/// One point of the water plane over a terrain, from [`WaterPlane::probe`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterProbe {
    /// Height of the rippled surface at the time asked, meters.
    pub surface_height: f32,
    /// Height of the terrain (the bilinear sim surface), meters.
    pub ground_height: f32,
    /// Still-water depth over the ground, `level − ground`, meters (negative on dry land).
    pub depth: f32,
    /// Whether the plane is drawn here (`depth >= -SHORE_CUTOFF`).
    pub wet: bool,
    /// Bed slope, rise over run.
    pub slope: f32,
    /// Horizontal distance to the shoreline, meters (0 on land).
    pub shore_distance: f32,
    /// Foam coverage before the mottle (the band's envelope), `0..=FOAM_COVERAGE`.
    pub foam: f32,
    /// Opacity of the water body seen from straight above (`0..=alpha`), times the shore fade.
    pub opacity_from_above: f32,
}

/// Errors returned for invalid water planes.
#[derive(Clone, Debug, Error)]
pub enum WaterError {
    /// Colors must be finite with components in 0..=1.
    #[error("water color components must be finite and in 0..=1, got {value:?}")]
    BadColour {
        /// Field name.
        field: &'static str,
        /// The offending color.
        value: [f32; 3],
    },
    /// Widths/depths/amplitudes must be finite and non-negative.
    #[error("water field '{field}' must be finite and >= 0, got {value}")]
    BadParameter {
        /// Field name.
        field: &'static str,
        /// The offending value.
        value: f32,
    },
    /// Alpha must be in 0..=1.
    #[error("water alpha must be in 0..=1, got {0}")]
    BadAlpha(f32),
    /// Water level must be finite.
    #[error("water level must be finite, got {0}")]
    BadLevel(f32),
}

/// Shoreline water plane parameters (`.water.ron`-able; part of the terrain asset, format
/// documented in `docs/formats/terrain.md`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaterPlane {
    /// World Y of the water surface, meters.
    pub level: f32,
    /// Color in shallow water (linear RGB).
    pub shallow_color: [f32; 3],
    /// Color in deep water (linear RGB).
    pub deep_color: [f32; 3],
    /// Color of the foam band (linear RGB).
    pub foam_color: [f32; 3],
    /// Width of the foam band from the shoreline, meters.
    pub foam_width: f32,
    /// Depth at which the color reaches `deep_color`, meters.
    pub max_depth: f32,
    /// Base transparency (alpha of deep water).
    pub alpha: f32,
    /// Ripple amplitude, meters.
    pub wave_amplitude: f32,
    /// Ripple spatial frequency (radians per meter).
    pub wave_frequency: f32,
}

impl Default for WaterPlane {
    fn default() -> Self {
        Self {
            level: 4.0,
            shallow_color: [0.10, 0.35, 0.38],
            deep_color: [0.02, 0.10, 0.16],
            // Muted so sunlit foam stays inside the tonemap's working range (a 0.9-linear
            // foam saturates whole shoreline bands to flat white under EV100-exposed sun).
            foam_color: [0.42, 0.46, 0.48],
            foam_width: 1.6,
            max_depth: 12.0,
            alpha: 0.75,
            wave_amplitude: 0.05,
            wave_frequency: 0.9,
        }
    }
}

impl WaterPlane {
    /// Validates the parameters.
    ///
    /// # Errors
    ///
    /// See [`WaterError`].
    pub fn validate(&self) -> Result<(), WaterError> {
        let colour = |field: &'static str, value: [f32; 3]| {
            let ok = value.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c));
            if ok {
                None
            } else {
                Some(WaterError::BadColour { field, value })
            }
        };
        if let Some(err) = colour("shallow_color", self.shallow_color) {
            return Err(err);
        }
        if let Some(err) = colour("deep_color", self.deep_color) {
            return Err(err);
        }
        if let Some(err) = colour("foam_color", self.foam_color) {
            return Err(err);
        }
        for (field, value) in [
            ("foam_width", self.foam_width),
            ("max_depth", self.max_depth),
            ("wave_amplitude", self.wave_amplitude),
            ("wave_frequency", self.wave_frequency),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(WaterError::BadParameter { field, value });
            }
        }
        if !self.alpha.is_finite() || !(0.0..=1.0).contains(&self.alpha) {
            return Err(WaterError::BadAlpha(self.alpha));
        }
        if !self.level.is_finite() {
            return Err(WaterError::BadLevel(self.level));
        }
        Ok(())
    }

    /// Serialises to a RON string.
    ///
    /// # Errors
    ///
    /// See [`WaterPlane::validate`] and [`crate::TerrainError::Ron`].
    pub fn to_ron(&self) -> Result<String, crate::TerrainError> {
        self.validate()?;
        // `\n` on every OS (ron's default is `\r\n` on Windows), so files match across hosts.
        Ok(ron::ser::to_string_pretty(self, regia_core::ron_pretty())?)
    }

    /// Parses and validates a RON string.
    ///
    /// # Errors
    ///
    /// See [`WaterPlane::validate`] and [`crate::TerrainError::Ron`].
    pub fn from_ron(text: &str) -> Result<Self, crate::TerrainError> {
        let parsed: Self = ron::from_str(text)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// The two ripples' phases at world `(x, z)` and `time` seconds.
    fn wave_phases(&self, x: f32, z: f32, time: f32) -> [f32; 2] {
        let k = self.wave_frequency;
        [
            (x * WAVE_DIRS[0][0] + z * WAVE_DIRS[0][1]) * k + time * WAVE_SPEED[0],
            (x * WAVE_DIRS[1][0] + z * WAVE_DIRS[1][1]) * k + time * WAVE_SPEED[1],
        ]
    }

    /// Wave numbers (radians per meter) of the two ripples.
    #[must_use]
    pub fn wave_numbers(&self) -> [f32; 2] {
        WAVE_DIRS.map(|[a, b]| (a * a + b * b).sqrt() * self.wave_frequency)
    }

    /// Height of the rippled surface at world `(x, z)` and `time` seconds: the still `level`
    /// plus the two ripples. The shading normal follows this surface everywhere; the drawn
    /// grid keeps only the ripples it resolves ([`WaterPlane::drawn_wave_fades`]).
    #[must_use]
    pub fn surface_height(&self, x: f32, z: f32, time: f32) -> f32 {
        let phases = self.wave_phases(x, z, time);
        self.level
            + phases[0].sin() * self.wave_amplitude * WAVE_GAIN[0]
            + phases[1].sin() * self.wave_amplitude * WAVE_GAIN[1]
    }

    /// Unit normal of the rippled surface at world `(x, z)` and `time` (analytic derivatives
    /// of [`WaterPlane::surface_height`]).
    #[must_use]
    pub fn surface_normal(&self, x: f32, z: f32, time: f32) -> [f32; 3] {
        let phases = self.wave_phases(x, z, time);
        let k = self.wave_frequency;
        let mut dydx = 0.0;
        let mut dydz = 0.0;
        for ((phase, gain), [dx, dz]) in phases.iter().zip(WAVE_GAIN).zip(WAVE_DIRS) {
            let slope = phase.cos() * k * self.wave_amplitude * gain;
            dydx += slope * dx;
            dydz += slope * dz;
        }
        let len = (dydx * dydx + 1.0 + dydz * dydz).sqrt();
        [-dydx / len, 1.0 / len, -dydz / len]
    }

    /// Displacement each ripple keeps on a drawn grid of `cell_size`-meter cells
    /// ([`vertex_wave_fade`]).
    #[must_use]
    pub fn drawn_wave_fades(&self, cell_size: f32) -> [f32; 2] {
        self.wave_numbers().map(|k| vertex_wave_fade(cell_size, k))
    }

    /// Opacity of the water body over `path` meters of water (Beer–Lambert): 0 at no water,
    /// `alpha` in the limit, 95 % of `alpha` at half `max_depth`.
    #[must_use]
    pub fn body_opacity(&self, path: f32) -> f32 {
        let tau = ABSORPTION_AT_MAX_DEPTH * path.max(0.0) / self.max_depth.max(0.01);
        self.alpha * (1.0 - (-tau).exp())
    }

    /// Color of the water body over `path` meters of water: `shallow_color` at none,
    /// `deep_color` from `max_depth` on.
    #[must_use]
    pub fn body_color(&self, path: f32) -> [f32; 3] {
        let t = (path.max(0.0) / self.max_depth.max(0.01)).min(1.0);
        std::array::from_fn(|i| self.shallow_color[i] + (self.deep_color[i] - self.shallow_color[i]) * t)
    }

    /// Foam coverage `shore_distance` meters from the shoreline with mottle noise `mottle`
    /// (`0..=1`): [`FOAM_COVERAGE`] at the waterline, gone at `foam_width` meters.
    #[must_use]
    pub fn foam(&self, shore_distance: f32, mottle: f32) -> f32 {
        let band = 1.0 - smoothstep(0.0, self.foam_width.max(0.05), shore_distance);
        (band * (0.55 + 0.45 * mottle.clamp(0.0, 1.0))).clamp(0.0, 1.0) * FOAM_COVERAGE
    }

    /// Shades one point of the surface: reflection, transmission, foam and the shore fade
    /// composed as `gpu/water.wgsl` does (before fog and the display transform). Returns
    /// `None` where the plane is not drawn (`depth < -SHORE_CUTOFF`).
    #[must_use]
    pub fn shade(&self, input: &WaterShadeInput) -> Option<WaterShade> {
        if input.depth < -SHORE_CUTOFF {
            return None;
        }
        let reflectance = interface_reflectance(input.cos_view);
        let (body, body_opacity) = if input.cos_view >= 0.0 {
            let path = optical_path(input.depth, input.cos_view);
            (self.body_color(path), self.body_opacity(path))
        } else {
            // From below the surface reflects the water body; the eye looks through the
            // water between itself and the surface.
            (self.deep_color, self.body_opacity(input.eye_distance))
        };
        let lit_body: [f32; 3] = std::array::from_fn(|i| body[i] * input.irradiance[i]);
        let lit_foam: [f32; 3] = std::array::from_fn(|i| self.foam_color[i] * input.irradiance[i]);
        let (premultiplied, alpha) = if input.cos_view >= 0.0 {
            let transmitted = (1.0 - reflectance) * body_opacity;
            let c: [f32; 3] = std::array::from_fn(|i| {
                reflectance * input.sky[i] + transmitted * lit_body[i] + input.glint[i]
            });
            (c, reflectance + transmitted)
        } else {
            let a = reflectance + (1.0 - reflectance) * body_opacity;
            let c: [f32; 3] = std::array::from_fn(|i| a * lit_body[i]);
            (c, a)
        };
        let foam = if input.cos_view >= 0.0 {
            self.foam(input.shore_distance, input.mottle)
        } else {
            0.0
        };
        let covered: [f32; 3] = std::array::from_fn(|i| (1.0 - foam) * premultiplied[i] + foam * lit_foam[i]);
        let covered_alpha = (1.0 - foam) * alpha + foam;
        let color = covered.map(|c| c / covered_alpha.max(1e-4));
        Some(WaterShade {
            color,
            alpha: (covered_alpha * shore_fade(input.depth)).clamp(0.0, 1.0),
            reflectance,
            body_opacity,
            foam,
        })
    }

    /// The water at world `(x, z)` over `map` at `time` seconds: surface and ground heights,
    /// depth, whether the plane is drawn there, the distance to the shoreline, the foam band
    /// and the opacity seen from straight above.
    #[must_use]
    pub fn probe(&self, map: &TerrainMap, x: f32, z: f32, time: f32) -> WaterProbe {
        let ground_height = map.sample_bilinear(x, z);
        let depth = self.level - ground_height;
        let slope = bed_slope(map, x, z);
        let shore = shore_distance(depth, slope);
        let wet = depth >= -SHORE_CUTOFF;
        WaterProbe {
            surface_height: self.surface_height(x, z, time),
            ground_height,
            depth,
            wet,
            slope,
            shore_distance: shore,
            foam: if wet { self.foam(shore, 1.0) } else { 0.0 },
            opacity_from_above: if wet {
                self.body_opacity(optical_path(depth, 1.0)) * shore_fade(depth)
            } else {
                0.0
            },
        }
    }
}

/// Bed slope (rise over run) at world `(x, z)`: central differences one sample apart, as the
/// water shader takes them from the height texture.
#[must_use]
pub fn bed_slope(map: &TerrainMap, x: f32, z: f32) -> f32 {
    let step = map.sample_spacing();
    let dx = (map.sample_bilinear(x + step, z) - map.sample_bilinear(x - step, z)) / (2.0 * step);
    let dz = (map.sample_bilinear(x, z + step) - map.sample_bilinear(x, z - step)) / (2.0 * step);
    (dx * dx + dz * dz).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid() {
        assert!(WaterPlane::default().validate().is_ok());
    }

    #[test]
    fn rejects_out_of_range_parameters() {
        let mut water = WaterPlane {
            deep_color: [0.0, 1.4, 0.0],
            ..Default::default()
        };
        assert!(matches!(
            water.validate(),
            Err(WaterError::BadColour {
                field: "deep_color",
                ..
            })
        ));
        water.deep_color = [0.0, 0.1, 0.0];
        water.foam_width = -1.0;
        assert!(matches!(
            water.validate(),
            Err(WaterError::BadParameter {
                field: "foam_width",
                ..
            })
        ));
        water.foam_width = 1.0;
        water.alpha = 1.2;
        assert!(matches!(water.validate(), Err(WaterError::BadAlpha(_))));
        water.alpha = 0.8;
        water.level = f32::INFINITY;
        assert!(matches!(water.validate(), Err(WaterError::BadLevel(_))));
    }

    #[test]
    fn ron_round_trip() {
        let water = WaterPlane::default();
        let text = water.to_ron().expect("serialize");
        assert_eq!(WaterPlane::from_ron(&text).expect("parse"), water);
    }

    /// A deep, open-water input: no foam (far from the shore), sky brighter than the body.
    fn open_water(depth: f32, cos_view: f32) -> WaterShadeInput {
        WaterShadeInput {
            depth,
            shore_distance: 1.0e3,
            cos_view,
            eye_distance: 10.0,
            mottle: 0.5,
            irradiance: [1.2, 1.2, 1.2],
            sky: [0.40, 0.50, 0.70],
            glint: [0.0; 3],
        }
    }

    /// A ramp map: the ground rises along +X at `slope` (rise over run), crossing the default
    /// water level 4 m at x = 0. 129 samples over 128 m.
    fn ramp(slope: f32) -> TerrainMap {
        let n = 129u32;
        let heights = (0..n * n)
            .map(|i| {
                let col = (i % n) as f32;
                4.0 + (col - 64.0) * slope
            })
            .collect();
        TerrainMap::from_heights(n, n, heights, 128.0, 64.0, -32.0).expect("ramp map")
    }

    #[test]
    fn f0_is_the_air_water_reflectance() {
        let r = (WATER_IOR - 1.0) / (WATER_IOR + 1.0);
        assert!((r * r - WATER_F0).abs() < 1e-6, "{} vs {WATER_F0}", r * r);
        assert!((fresnel_schlick(1.0) - WATER_F0).abs() < 1e-7);
        assert!((fresnel_schlick(0.0) - 1.0).abs() < 1e-6);
        let mut last = 1.0;
        for i in 0..=100 {
            let f = fresnel_schlick(i as f32 / 100.0);
            assert!(f <= last + 1e-7, "Fresnel must fall toward normal incidence");
            last = f;
        }
    }

    #[test]
    fn refraction_obeys_snell() {
        assert!((refracted_cos(1.0) - 1.0).abs() < 1e-6);
        for i in 0..=20 {
            let cos_i = i as f32 / 20.0;
            let cos_t = refracted_cos(cos_i);
            let sin_i = (1.0 - cos_i * cos_i).sqrt();
            let sin_t = (1.0 - cos_t * cos_t).sqrt();
            assert!((sin_i - WATER_IOR * sin_t).abs() < 1e-4, "Snell at cos {cos_i}");
            assert!(cos_t >= 0.66, "the refracted ray never passes the critical angle");
        }
        // The light seen at a grazing angle crossed more water than the light seen from above,
        // at most 1/cos(critical) as much.
        assert!(optical_path(2.0, 0.1) > optical_path(2.0, 1.0));
        assert!(optical_path(2.0, 0.0) <= 2.0 / 0.661);
        assert_eq!(optical_path(-1.0, 0.5), 0.0);
    }

    #[test]
    fn total_internal_reflection_from_below() {
        // Looking straight up from inside the water: the window transmits.
        assert!((interface_reflectance(-1.0) - WATER_F0).abs() < 1e-6);
        // Past the critical angle (asin(1/n) ≈ 48.6°) the surface is a mirror.
        let critical = (1.0 / WATER_IOR).asin();
        let past = (critical + 0.05).cos();
        let inside = (critical - 0.05).cos();
        assert_eq!(interface_reflectance(-past), 1.0);
        assert!(interface_reflectance(-inside) < 1.0);
        let below = WaterPlane::default()
            .shade(&open_water(5.0, -past))
            .expect("drawn");
        assert!((below.alpha - 1.0).abs() < 1e-6, "{below:?}");
        assert_eq!(below.foam, 0.0, "no foam seen from below");
    }

    #[test]
    fn the_waterline_has_no_step() {
        // The defect this pins: opacity was floored at 0.35 · alpha up to the cutoff and
        // then discarded, a visible hard edge along every shoreline.
        assert_eq!(shore_fade(-SHORE_CUTOFF), 0.0);
        assert_eq!(shore_fade(SHORE_FADE - SHORE_CUTOFF), 1.0);
        let water = WaterPlane::default();
        let input = |depth: f32| WaterShadeInput {
            shore_distance: shore_distance(depth, 0.1),
            ..open_water(depth, 0.8)
        };
        assert!(water.shade(&input(-SHORE_CUTOFF - 1e-3)).is_none(), "dry land");
        let first = water.shade(&input(-SHORE_CUTOFF)).expect("drawn at the cutoff");
        assert!(first.alpha < 1e-6, "the drawn edge starts transparent: {first:?}");
        let mut last = first.alpha;
        let mut depth = -SHORE_CUTOFF;
        while depth < 2.0 {
            depth += 1e-3;
            let shade = water.shade(&input(depth)).expect("drawn");
            assert!(
                (shade.alpha - last).abs() < 0.02,
                "opacity jumps by {} at depth {depth}",
                shade.alpha - last
            );
            last = shade.alpha;
        }
    }

    #[test]
    fn grazing_views_reflect_the_sky() {
        let water = WaterPlane::default();
        let above = water.shade(&open_water(30.0, 1.0)).expect("drawn");
        let grazing = water.shade(&open_water(30.0, 0.03)).expect("drawn");
        assert!(above.reflectance < 0.03 && grazing.reflectance > 0.8);
        assert!(grazing.alpha > above.alpha, "{grazing:?} vs {above:?}");
        // At grazing the color is mostly the sky's.
        let sky = [0.40, 0.50, 0.70];
        let dist = |c: [f32; 3]| c.iter().zip(sky).map(|(a, b)| (a - b).abs()).sum::<f32>();
        assert!(
            dist(grazing.color) < dist(above.color),
            "{grazing:?} vs {above:?}"
        );
        assert!(grazing.color[2] > grazing.color[0], "a blue sky reflects blue");
    }

    #[test]
    fn shallow_water_shows_the_bed() {
        let water = WaterPlane::default();
        let shallow = water.shade(&open_water(0.3, 1.0)).expect("drawn");
        let deep = water
            .shade(&open_water(water.max_depth * 0.5, 1.0))
            .expect("drawn");
        assert!(shallow.body_opacity < 0.15 * water.alpha, "{shallow:?}");
        assert!((deep.body_opacity - 0.95 * water.alpha).abs() < 0.01, "{deep:?}");
        assert!(shallow.alpha < deep.alpha);
        let mut last = 0.0;
        for i in 0..200 {
            let opacity = water.body_opacity(i as f32 * 0.25);
            assert!((last..=water.alpha).contains(&opacity));
            last = opacity;
        }
        assert_eq!(water.body_color(0.0), water.shallow_color);
        let deep_color = water.body_color(water.max_depth * 2.0);
        assert!(
            deep_color
                .iter()
                .zip(water.deep_color)
                .all(|(a, b)| (a - b).abs() < 1e-6),
            "{deep_color:?}"
        );
    }

    #[test]
    fn alpha_and_energy_stay_in_range() {
        let water = WaterPlane {
            alpha: 1.0,
            ..WaterPlane::default()
        };
        for d in 0..40 {
            for c in -20..=20 {
                let input = WaterShadeInput {
                    shore_distance: d as f32 * 0.1,
                    ..open_water(d as f32 * 0.5 - 0.05, c as f32 / 20.0)
                };
                let shade = water.shade(&input).expect("drawn");
                assert!((0.0..=1.0).contains(&shade.alpha), "{shade:?}");
                assert!(shade.reflectance + (1.0 - shade.reflectance) * shade.body_opacity <= 1.0 + 1e-6);
                assert!(
                    shade.color.iter().all(|c| c.is_finite() && *c >= 0.0),
                    "{shade:?}"
                );
            }
        }
    }

    #[test]
    fn foam_width_is_measured_from_the_shoreline() {
        // The defect this pins: the band was `foam_width` meters of *depth*, so on a 1:10
        // beach a 1.6 m band printed 16 m wide.
        let water = WaterPlane::default();
        let map = ramp(-0.1); // Ground falls toward +X: water on the +X side.
        let near = water.probe(&map, 1.0, 0.0, 0.0);
        let far = water.probe(&map, 2.5, 0.0, 0.0);
        assert!(near.wet && far.wet);
        assert!((near.slope - 0.1).abs() < 1e-3, "{near:?}");
        assert!((near.shore_distance - 1.0).abs() < 0.02, "{near:?}");
        assert!(near.foam > 0.0, "{near:?}");
        assert!(far.depth < water.foam_width, "the old depth band would foam here");
        assert_eq!(far.foam, 0.0, "{far:?}");
        assert_eq!(water.foam(water.foam_width, 1.0), 0.0);
        assert!((water.foam(0.0, 1.0) - FOAM_COVERAGE).abs() < 1e-6);
        // Dry land.
        let land = water.probe(&map, -3.0, 0.0, 0.0);
        assert!(
            !land.wet && land.foam == 0.0 && land.opacity_from_above == 0.0,
            "{land:?}"
        );
        // A flat bed does not divide by zero.
        assert!(shore_distance(1.0, 0.0).is_finite());
        assert!(shore_distance(1.0, f32::NAN).is_finite());
    }

    #[test]
    fn coarse_grids_drop_unresolved_ripples() {
        let water = WaterPlane::default();
        // Fine cells keep both ripples.
        assert_eq!(water.drawn_wave_fades(0.25), [1.0, 1.0]);
        // A DO27-sized 8 km map spreads 64 cells 125 m apart: neither ripple is resolved.
        assert_eq!(water.drawn_wave_fades(8000.0 / WATER_GRID as f32), [0.0, 0.0]);
        // Monotonic in the cell size.
        let mut last = 1.0;
        for i in 0..100 {
            let fade = vertex_wave_fade(i as f32 * 0.1, water.wave_numbers()[1]);
            assert!(fade <= last);
            last = fade;
        }
        // Exactly at Nyquist (two vertices per wavelength) nothing is kept.
        let k = water.wave_numbers()[0];
        assert!(vertex_wave_fade(std::f32::consts::PI / k, k) < 1e-5);
    }

    #[test]
    fn normal_is_the_surface_gradient() {
        let water = WaterPlane {
            wave_amplitude: 0.3,
            ..WaterPlane::default()
        };
        let e = 1e-2;
        for (x, z, t) in [(0.0, 0.0, 0.0), (3.7, -11.2, 4.5), (-40.0, 7.5, 12.25)] {
            let dx = (water.surface_height(x + e, z, t) - water.surface_height(x - e, z, t)) / (2.0 * e);
            let dz = (water.surface_height(x, z + e, t) - water.surface_height(x, z - e, t)) / (2.0 * e);
            let n = water.surface_normal(x, z, t);
            assert!((n[0] / n[1] + dx).abs() < 2e-3, "dx at ({x}, {z}): {n:?} vs {dx}");
            assert!((n[2] / n[1] + dz).abs() < 2e-3, "dz at ({x}, {z}): {n:?} vs {dz}");
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-5);
        }
        let still = WaterPlane {
            wave_amplitude: 0.0,
            ..WaterPlane::default()
        };
        assert_eq!(still.surface_height(5.0, 5.0, 1.0), still.level);
        assert_eq!(still.surface_normal(5.0, 5.0, 1.0), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn the_refracted_bed_point_obeys_snell() {
        let up = [0.0, 1.0, 0.0];
        // Straight down: the bed point is right below.
        let below = refracted_bed_point([3.0, 4.0, -2.0], up, up, 5.0);
        let expected = [3.0, -1.0, -2.0];
        assert!(
            below.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-5),
            "{below:?}"
        );
        // At 45° the ray bends toward the normal: the bed point is nearer than the straight
        // ray's (apparent depth), with sin θi = n · sin θt.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let eye = [-s, s, 0.0]; // The eye is up and to −X; the ray travels toward +X.
        let bed = refracted_bed_point([0.0, 0.0, 0.0], eye, up, 2.0);
        assert!((bed[1] + 2.0).abs() < 1e-5, "{bed:?}");
        assert!(
            bed[0] > 0.0 && bed[0] < 2.0,
            "nearer than the straight ray's 2 m: {bed:?}"
        );
        let tan_t = bed[0] / 2.0;
        let sin_t = tan_t / (1.0 + tan_t * tan_t).sqrt();
        assert!(
            (s - WATER_IOR * sin_t).abs() < 1e-4,
            "Snell: {s} vs {}",
            WATER_IOR * sin_t
        );
        assert!(bed[2].abs() < 1e-6);
        // A ripple normal turned away from a grazing eye refracts about the still plane's:
        // the bed point stays below the surface and near it, not thrown upward.
        let grazing = [-0.999, 0.0447, 0.0];
        let away = [0.3, 0.954, 0.0];
        let tilted = refracted_bed_point([0.0, 0.0, 0.0], grazing, away, 1.0);
        let still = refracted_bed_point([0.0, 0.0, 0.0], grazing, up, 1.0);
        assert_eq!(tilted, still);
        assert!(
            (still[1] + 1.0).abs() < 1e-5 && still[0] > 0.0 && still[0] < 1.2,
            "{still:?}"
        );
        // No water, no offset.
        assert_eq!(
            refracted_bed_point([1.0, 2.0, 3.0], eye, up, 0.0),
            [1.0, 2.0, 3.0]
        );
        assert_eq!(
            refracted_bed_point([1.0, 2.0, 3.0], eye, up, -1.0),
            [1.0, 2.0, 3.0]
        );
    }

    /// The WGSL constant `name` parsed from `gpu/water.wgsl`.
    fn wgsl_const(name: &str) -> f32 {
        let source = include_str!("gpu/water.wgsl");
        let prefix = format!("const {name}: f32 = ");
        let line = source
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("water.wgsl has no `{prefix}`"));
        line[prefix.len()..]
            .trim_end_matches(';')
            .parse()
            .unwrap_or_else(|e| panic!("{line}: {e}"))
    }

    #[test]
    fn shader_constants_mirror_the_reference() {
        for (name, value) in [
            ("WATER_IOR", WATER_IOR),
            ("WATER_F0", WATER_F0),
            ("SHORE_CUTOFF", SHORE_CUTOFF),
            ("SHORE_FADE", SHORE_FADE),
            ("ABSORPTION_AT_MAX_DEPTH", ABSORPTION_AT_MAX_DEPTH),
            ("FOAM_COVERAGE", FOAM_COVERAGE),
            ("MIN_SHORE_SLOPE", MIN_SHORE_SLOPE),
        ] {
            assert_eq!(wgsl_const(name), value, "water.wgsl {name}");
        }
        let source = include_str!("gpu/water.wgsl");
        assert!(source.contains(&format!("const GRID: u32 = {WATER_GRID}u;")));
        // The two ripples' directions, speeds and gains as the shader writes them.
        for needle in [
            "vec2<f32>(0.7, 0.3)",
            "vec2<f32>(-0.78, 1.3)",
            "vec2<f32>(1.1, 0.9)",
            "vec2<f32>(1.0, 0.6)",
        ] {
            assert!(source.contains(needle), "water.wgsl lost `{needle}`");
        }
        assert_eq!(WAVE_DIRS[1], [-0.78, 1.3]);
        assert_eq!(WAVE_SPEED, [1.1, 0.9]);
        assert_eq!(WAVE_GAIN, [1.0, 0.6]);
    }
}
