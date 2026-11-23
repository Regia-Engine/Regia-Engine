// SPDX-License-Identifier: MPL-2.0
//! Froxel volumetric fog (`specs/26` § 4).
//!
//! Each froxel stores in-scatter and extinction. Light comes from the sun (shadowed by a
//! cascade shadow map) and from a few local lights. Slices are integrated front to back
//! and blended with the previous frame after reprojection.
//!
//! Wronski, "Volumetric Fog: Unified compute shader based solution to atmospheric
//! scattering", SIGGRAPH 2014 Advances in Real-Time Rendering. Henyey–Greenstein phase:
//! Henyey & Greenstein, Astrophysical Journal 1941.

use regia_app::prelude::*;

/// Axis-aligned box or sphere of extra fog density (1/metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FogVolume {
    /// Box centred at `center` with half-extents `extents` (metres).
    Box {
        /// World-space centre.
        center: [f32; 3],
        /// Half-size on each axis (metres).
        extents: [f32; 3],
        /// Extinction added inside the box (1/metres).
        density: f32,
    },
    /// Sphere.
    Sphere {
        /// World-space centre.
        center: [f32; 3],
        /// Radius (metres).
        radius: f32,
        /// Extinction added inside the sphere (1/metres).
        density: f32,
    },
}

impl FogVolume {
    /// Fog that exists only inside `interior` (ADR-3257): a [`Self::Box`] with the interior's
    /// centre and half extents, so the froxel grid adds `density` exactly where
    /// [`crate::InteriorVolume::contains`] holds (a tunnel's dust, a cave's haze). An interior
    /// that contains no point (a non-positive or non-finite axis, a non-finite centre) gives a
    /// volume of density 0, and a negative or non-finite `density` adds none, so the fog never
    /// leaks past the interior it is bounded by.
    #[must_use]
    pub fn interior(interior: crate::InteriorVolume, density: f32) -> Self {
        let valid = interior
            .half_extent
            .iter()
            .all(|axis| axis.is_finite() && *axis > 0.0)
            && interior.center.iter().all(|c| c.is_finite());
        let density = if valid && density.is_finite() {
            density.max(0.0)
        } else {
            0.0
        };
        Self::Box {
            center: interior.center,
            extents: interior.half_extent,
            density,
        }
    }

    /// Density contributed at `point`, or zero outside the volume.
    #[must_use]
    pub fn density_at(self, point: [f32; 3]) -> f32 {
        match self {
            Self::Box {
                center,
                extents,
                density,
            } => {
                let inside = (0..3).all(|c| (point[c] - center[c]).abs() <= extents[c].abs());
                if inside { density.max(0.0) } else { 0.0 }
            }
            Self::Sphere {
                center,
                radius,
                density,
            } => {
                let d2 = (0..3).map(|c| point[c] - center[c]).map(|d| d * d).sum::<f32>();
                if d2 <= radius.max(0.0) * radius.max(0.0) {
                    density.max(0.0)
                } else {
                    0.0
                }
            }
        }
    }
}

/// Participating-media settings for the froxel grid.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct VolumetricFogSettings {
    /// Height-fog extinction at [`Self::base_height`] (1/metres). Zero leaves only volumes.
    pub density: f32,
    /// Exponential height falloff (1/metres).
    pub height_falloff: f32,
    /// World Y where [`Self::density`] applies (metres).
    pub base_height: f32,
    /// Scatter colour, each channel in `0..1`.
    pub albedo: [f32; 3],
    /// Henyey–Greenstein anisotropy, `-1..1`. Positive is forward scattering.
    pub anisotropy: f32,
    /// Weight of the previous frame after reprojection (`0` disables history).
    pub history_weight: f32,
    /// On a planet whose air is not Earth's (Mars), scatter like that planet's aerosol:
    /// the preset's dust single-scattering albedo and mean asymmetry replace
    /// [`Self::albedo`] and [`Self::anisotropy`] (`OutdoorParams::fog_medium`). Martian
    /// haze is dust that absorbs blue, not white water fog; a white medium under the
    /// butterscotch sky printed as a lavender-grey wall. Earth and airless presets keep
    /// the authored values. Default on.
    pub planet_medium: bool,
    /// How the fog reads the sun shadow (ADR-3122). Default
    /// [`FogSunShadowRule::Legacy`](crate::fog::FogSunShadowRule::Legacy), the v0.2 look;
    /// [`FogSunShadowRule::Calibrated`](crate::fog::FogSunShadowRule::Calibrated) keeps the
    /// sun out of the air under a roof (spec 27 § 2.2 fog rows) and moves fogged frames.
    pub sun_shadow: crate::fog::FogSunShadowRule,
}

impl Default for VolumetricFogSettings {
    fn default() -> Self {
        Self {
            density: 0.0,
            height_falloff: 0.04,
            base_height: 0.0,
            albedo: [0.85, 0.88, 0.92],
            anisotropy: 0.35,
            history_weight: 0.85,
            planet_medium: true,
            sun_shadow: crate::fog::FogSunShadowRule::Legacy,
        }
    }
}

/// List of fog volumes. Empty means height fog only (or nothing, if that density is zero).
#[derive(Resource, Debug, Clone, Default)]
pub struct FogVolumes {
    /// Volumes summed into the froxel density.
    pub volumes: Vec<FogVolume>,
}

/// One local light the fog march can see. Intensity is luminous intensity (candela) for both
/// kinds, the quantity the forward shader multiplies by inverse-square falloff. A point light's
/// lumens convert with `lm / 4π` (`KHR_lights_punctual` § 5.3); [`FogLight::from_local`] does it.
///
/// Build it with [`FogLight::point`] or [`FogLight::spot`]: fields may be added (v0.2.0 added
/// [`Self::cone`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogLight {
    /// World position (metres).
    pub position: [f32; 3],
    /// Linear RGB.
    pub color: [f32; 3],
    /// Candela, multiplied by the windowed inverse-square falloff ([`fog_light_falloff`]).
    pub intensity: f32,
    /// Radius of the windowed inverse-square falloff (metres).
    pub radius: f32,
    /// The spot cone, `None` for a point light (lights every direction).
    pub cone: Option<FogCone>,
}

/// A spot light's cone in the fog, the same linear ramp the forward pass applies
/// (`spot_attenuation` in `pbr_lighting.wgsl`): full inside the inner half-angle, zero
/// outside the outer one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogCone {
    /// Direction from the light into the scene (unit length).
    pub direction: [f32; 3],
    /// `cos` of the inner half-angle.
    pub cos_inner: f32,
    /// `cos` of the outer half-angle.
    pub cos_outer: f32,
}

impl FogCone {
    /// Cone term for a sample whose unit direction toward the light is `to_light`.
    #[must_use]
    pub fn factor(&self, to_light: [f32; 3]) -> f32 {
        let cos_angle = -dot(unit(self.direction), to_light);
        let denom = (self.cos_inner - self.cos_outer).max(1.0e-4);
        ((cos_angle - self.cos_outer) / denom).clamp(0.0, 1.0)
    }

    /// GPU packing: `xyz = −direction · s`, `w = −cos_outer · s` with
    /// `s = 1 / max(cos_inner − cos_outer, 1e-4)`, so the shader's cone term is
    /// `saturate(dot(xyz, to_light) + w)`, the same ramp as [`Self::factor`].
    #[must_use]
    pub fn gpu(&self) -> [f32; 4] {
        let d = unit(self.direction);
        let s = 1.0 / (self.cos_inner - self.cos_outer).max(1.0e-4);
        [-d[0] * s, -d[1] * s, -d[2] * s, -self.cos_outer * s]
    }
}

impl FogLight {
    /// A point light: `intensity_cd` in candela (lumens / 4π).
    #[must_use]
    pub fn point(position: [f32; 3], color: [f32; 3], intensity_cd: f32, radius: f32) -> Self {
        Self {
            position,
            color,
            intensity: intensity_cd,
            radius,
            cone: None,
        }
    }

    /// A spot light: `intensity_cd` in candela, `direction` from the light into the scene,
    /// cone half-angles in radians (as `SpotLight` / `LabSpotLight` hold them).
    #[must_use]
    pub fn spot(
        position: [f32; 3],
        direction: [f32; 3],
        color: [f32; 3],
        intensity_cd: f32,
        range: f32,
        inner_angle: f32,
        outer_angle: f32,
    ) -> Self {
        Self {
            position,
            color,
            intensity: intensity_cd,
            radius: range,
            cone: Some(FogCone {
                direction: unit(direction),
                cos_inner: inner_angle.cos(),
                cos_outer: outer_angle.cos(),
            }),
        }
    }

    /// Cone term toward a sample (1 for a point light). `to_light` is unit length.
    #[must_use]
    pub fn cone_factor(&self, to_light: [f32; 3]) -> f32 {
        self.cone.map_or(1.0, |cone| cone.factor(to_light))
    }

    /// The cone row of the fog uniform: [`FogCone::gpu`], or `(0, 0, 0, 1)` (term 1) for a
    /// point light.
    #[must_use]
    pub fn gpu_cone(&self) -> [f32; 4] {
        self.cone.map_or([0.0, 0.0, 0.0, 1.0], |cone| cone.gpu())
    }
}

/// Extinction at `point` from height fog plus every volume.
#[must_use]
pub fn extinction_at(point: [f32; 3], settings: &VolumetricFogSettings, volumes: &[FogVolume]) -> f32 {
    let height = settings.density.max(0.0)
        * (-settings.height_falloff.max(0.0) * (point[1] - settings.base_height)).exp();
    let extra: f32 = volumes.iter().map(|volume| volume.density_at(point)).sum();
    (height + extra).max(0.0)
}

/// Length (metres) of the paths [`fog_layer_optical_depth`] integrates. Mirrors
/// `FOG_LAYER_RANGE_M` in `volumetric_fog.wgsl`.
pub const FOG_LAYER_RANGE_M: f32 = 20_000.0;

/// Optical depth of the exponential height fog from `point` along the unit
/// direction `dir`, out to [`FOG_LAYER_RANGE_M`].
///
/// For a horizontally uniform layer `σ(y) = density · e^(−k (y − base))` the integral
/// along `p + t·d` is `σ(p) · (1 − e^(−k d_y L)) / (k d_y)`. A sun a few degrees up
/// crosses kilometres of ground haze (large depth), a high sun only the column above
/// the point. Volumes are not included. The froxel shader dims each sample's
/// sunlight by `exp(−depth)` toward the sun and its skylight by the diffuse
/// transmittance of the column above (`exp(−1.66 · depth_up)`, Elsasser's
/// diffusivity factor).
#[must_use]
pub fn fog_layer_optical_depth(point: [f32; 3], dir: [f32; 3], settings: &VolumetricFogSettings) -> f32 {
    let k = settings.height_falloff.max(0.0);
    let rho = (settings.density.max(0.0) * (-k * (point[1] - settings.base_height)).exp()).max(0.0);
    let a = k * dir[1];
    if (a * FOG_LAYER_RANGE_M).abs() < 1.0e-4 {
        return rho * FOG_LAYER_RANGE_M;
    }
    let x = (a * FOG_LAYER_RANGE_M).clamp(-30.0, 30.0);
    rho * (1.0 - (-x).exp()) / a
}

/// Henyey–Greenstein phase function (1/sr).
#[must_use]
pub fn henyey_greenstein(cos_theta: f32, g: f32) -> f32 {
    let g = g.clamp(-0.99, 0.99);
    let g2 = g * g;
    let denom = (1.0 + g2 - 2.0 * g * cos_theta).max(1.0e-4).powf(1.5);
    (1.0 - g2) / (4.0 * std::f32::consts::PI * denom)
}

/// Sun term for one froxel sample.
#[derive(Clone, Copy, Debug)]
pub struct SunScattering {
    /// Travel direction, from the sun toward the scene.
    pub travel: [f32; 3],
    /// Linear sun colour.
    pub color: [f32; 3],
    /// Illuminance in lux.
    pub illuminance: f32,
    /// 0 in shadow, 1 in light.
    pub shadow: f32,
}

/// In-scatter per metre (cd/m² / m) from the sun and local lights at one sample.
///
/// `view_dir` points from the camera toward the sample.
#[must_use]
pub fn inscatter_per_metre(
    point: [f32; 3],
    view_dir: [f32; 3],
    sun: &SunScattering,
    lights: &[FogLight],
    settings: &VolumetricFogSettings,
    volumes: &[FogVolume],
) -> [f32; 3] {
    let ext = extinction_at(point, settings, volumes);
    if ext <= 0.0 {
        return [0.0; 3];
    }
    let to_sun = [-sun.travel[0], -sun.travel[1], -sun.travel[2]];
    let cos_sun = dot(view_dir, to_sun).clamp(-1.0, 1.0);
    let phase = henyey_greenstein(cos_sun, settings.anisotropy);
    let sun_t = (-fog_layer_optical_depth(point, unit(to_sun), settings)).exp();
    let mut light = mul(
        sun.color,
        sun.illuminance.max(0.0) * phase * sun.shadow.clamp(0.0, 1.0) * sun_t,
    );
    for lamp in lights.iter().take(4) {
        let offset = sub(lamp.position, point);
        let dist = length(offset).max(1.0e-3);
        let falloff = fog_light_falloff(dist, lamp.radius);
        let to_light = mul(offset, 1.0 / dist);
        let phase_l = henyey_greenstein(dot(view_dir, to_light).clamp(-1.0, 1.0), settings.anisotropy);
        let scale = lamp.intensity.max(0.0) * falloff * phase_l * lamp.cone_factor(to_light);
        light = add(light, mul(lamp.color, scale));
    }
    [
        ext * settings.albedo[0].clamp(0.0, 1.0) * light[0],
        ext * settings.albedo[1].clamp(0.0, 1.0) * light[1],
        ext * settings.albedo[2].clamp(0.0, 1.0) * light[2],
    ]
}

/// Distance falloff of a local light in the fog: the forward pass's windowed inverse square,
/// `saturate(1 − (d / r)⁴)² / (d² + 1)` (`distance_attenuation` in `pbr_lighting.wgsl`; Lagarde,
/// Frostbite 2014). 0 when `radius` is not positive.
#[must_use]
pub fn fog_light_falloff(distance: f32, radius: f32) -> f32 {
    if radius <= 0.0 {
        return 0.0;
    }
    let d = distance.max(0.0);
    let window = (1.0 - (d / radius).powi(4)).clamp(0.0, 1.0);
    window * window / (d * d + 1.0)
}

/// Front-to-back integral of a homogeneous segment. Returns (inscatter, transmittance).
#[must_use]
pub fn integrate_segment(inscatter_per_m: [f32; 3], extinction: f32, length_m: f32) -> ([f32; 3], f32) {
    let length_m = length_m.max(0.0);
    let extinction = extinction.max(0.0);
    if length_m <= 0.0 {
        return ([0.0; 3], 1.0);
    }
    let optical = extinction * length_m;
    let transmittance = (-optical).exp();
    let integral = if optical < 1.0e-4 {
        length_m
    } else {
        (1.0 - transmittance) / extinction
    };
    (mul(inscatter_per_m, integral), transmittance)
}

/// Exponential froxel depth (metres along the view axis), Wronski's distribution.
#[must_use]
pub fn froxel_depth(slice: u32, slice_count: u32, near: f32, far: f32) -> f32 {
    let count = slice_count.max(1) as f32;
    let t = (slice as f32 + 0.5) / count;
    let near = near.max(0.01);
    let far = far.max(near + 0.01);
    near * (far / near).powf(t)
}

/// Reprojects a world point into the previous camera's clip UV and clip-space z (`0..1`).
///
/// U and V are the previous view's screen position. W is the clip-space depth `clip.z / clip.w`,
/// not a froxel slice. `near` and `far` are unused: view-axis depth from clip z is
/// projection-dependent, and this function does not receive the previous eye. A caller that
/// knows that eye uses [`froxel_slice_uvw`]. The fog history blend samples the log slice itself
/// and does not call this function.
///
/// `None` when the point is outside the previous frustum or behind it, which rejects history.
#[must_use]
pub fn reproject_froxel(
    world: [f32; 3],
    prev_view_proj: [[f32; 4]; 4],
    near: f32,
    far: f32,
) -> Option<[f32; 3]> {
    let clip = mul_mat4(prev_view_proj, [world[0], world[1], world[2], 1.0]);
    if clip[3].abs() < 1.0e-5 {
        return None;
    }
    let ndc_x = clip[0] / clip[3];
    let ndc_y = clip[1] / clip[3];
    let ndc_z = clip[2] / clip[3];
    if !(-1.0..=1.0).contains(&ndc_x) || !(-1.0..=1.0).contains(&ndc_y) || !(0.0..=1.0).contains(&ndc_z) {
        return None;
    }
    let u = ndc_x * 0.5 + 0.5;
    let v = 1.0 - (ndc_y * 0.5 + 0.5);
    // View-axis depth from the clip z is projection-dependent. Callers that know the
    // previous eye pass the distance through `froxel_slice_uvw` instead. Here the slice
    // coordinate is the clip-space depth, which is monotonic in the engine's 0..1 Z.
    let _ = (near, far);
    Some([u, v, ndc_z])
}

/// Slice coordinate `0..1` from a view-axis distance, matching [`froxel_depth`].
#[must_use]
pub fn froxel_slice_uvw(distance_m: f32, near: f32, far: f32) -> f32 {
    let near = near.max(0.01);
    let far = far.max(near + 0.01);
    let d = distance_m.clamp(near, far);
    (d / near).ln() / (far / near).ln()
}

/// History blend. `history_weight` is how much of the previous integrated result to keep.
#[must_use]
pub fn blend_history(
    current: [f32; 4],
    history: [f32; 4],
    history_weight: f32,
    history_valid: bool,
) -> [f32; 4] {
    if !history_valid {
        return current;
    }
    let w = history_weight.clamp(0.0, 0.95);
    [
        current[0] + (history[0] - current[0]) * w,
        current[1] + (history[1] - current[1]) * w,
        current[2] + (history[2] - current[2]) * w,
        current[3] + (history[3] - current[3]) * w,
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn mul(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

fn unit(a: [f32; 3]) -> [f32; 3] {
    let l = length(a);
    if l > 1.0e-8 {
        mul(a, 1.0 / l)
    } else {
        [0.0, 1.0, 0.0]
    }
}

fn mul_mat4(m: [[f32; 4]; 4], v: [f32; 4]) -> [f32; 4] {
    // Column-major, matching glam's `to_cols_array_2d`.
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2] + m[3][0] * v[3],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2] + m[3][1] * v[3],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2] + m[3][2] * v[3],
        m[0][3] * v[0] + m[1][3] * v[1] + m[2][3] * v[2] + m[3][3] * v[3],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> VolumetricFogSettings {
        VolumetricFogSettings {
            density: 0.0,
            ..VolumetricFogSettings::default()
        }
    }

    #[test]
    fn box_and_sphere_densities_are_local() {
        let box_v = FogVolume::Box {
            center: [0.0, 1.0, 0.0],
            extents: [1.0, 1.0, 2.0],
            density: 0.4,
        };
        let sphere = FogVolume::Sphere {
            center: [5.0, 1.0, 0.0],
            radius: 1.5,
            density: 0.2,
        };
        assert!((box_v.density_at([0.0, 1.0, 0.0]) - 0.4).abs() < 1.0e-6);
        assert_eq!(box_v.density_at([3.0, 1.0, 0.0]), 0.0);
        assert!((sphere.density_at([5.0, 1.0, 0.0]) - 0.2).abs() < 1.0e-6);
        assert_eq!(sphere.density_at([5.0, 1.0, 2.0]), 0.0);
        let sum = extinction_at([0.0, 1.0, 0.0], &settings(), &[box_v, sphere]);
        assert!((sum - 0.4).abs() < 1.0e-5);
    }

    #[test]
    fn interior_fog_exists_exactly_where_the_interior_contains_the_point() {
        let tube = crate::InteriorVolume::with_floor([0.0, -3.25, 40.0], [5.0, 2.75, 40.0]);
        let fog = FogVolume::interior(tube, 0.01);
        // A lattice through and around the box, edges included: density iff contained.
        for i in -8..=8 {
            for j in -8..=8 {
                for k in -8..=8 {
                    let p = [i as f32 * 0.75, -3.25 + j as f32 * 0.5, 40.0 + k as f32 * 6.0];
                    let expect = if tube.contains(p) { 0.01 } else { 0.0 };
                    assert_eq!(fog.density_at(p), expect, "at {p:?}");
                }
            }
        }
        assert_eq!(fog.density_at([5.0, -0.5, 80.0]), 0.01, "the corner is inside");
        assert_eq!(fog.density_at([5.001, -3.0, 40.0]), 0.0);
        // An interior that contains nothing adds no fog anywhere (a negative axis would turn
        // into a box through `abs`).
        for bad in [
            crate::InteriorVolume::with_floor([0.0; 3], [-5.0, 2.0, 2.0]),
            crate::InteriorVolume::with_floor([0.0; 3], [5.0, 0.0, 2.0]),
            crate::InteriorVolume::with_floor([0.0; 3], [5.0, f32::NAN, 2.0]),
            crate::InteriorVolume::with_floor([f32::INFINITY, 0.0, 0.0], [5.0, 2.0, 2.0]),
        ] {
            let fog = FogVolume::interior(bad, 0.5);
            assert_eq!(fog.density_at([0.0; 3]), 0.0);
            assert_eq!(fog.density_at([4.0, 1.0, 1.0]), 0.0);
        }
        for density in [-1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(FogVolume::interior(tube, density).density_at(tube.center), 0.0);
        }
    }

    #[test]
    fn shadow_kills_the_sun_and_a_local_light_falls_off() {
        let volume = FogVolume::Box {
            center: [0.0, 1.0, 0.0],
            extents: [4.0, 2.0, 4.0],
            density: 0.2,
        };
        let fog = settings();
        let view = [0.0, 0.0, -1.0];
        let sun = |shadow: f32, illuminance: f32| SunScattering {
            travel: [0.0, -1.0, 0.0],
            color: [1.0, 1.0, 1.0],
            illuminance,
            shadow,
        };
        let lit = inscatter_per_metre([0.0, 1.0, 0.0], view, &sun(1.0, 50_000.0), &[], &fog, &[volume]);
        let dark = inscatter_per_metre([0.0, 1.0, 0.0], view, &sun(0.0, 50_000.0), &[], &fog, &[volume]);
        assert!(lit[1] > 1.0, "lit fog scatters, {lit:?}");
        assert!(
            dark[1] < lit[1] * 0.05,
            "shadow removes the sun, dark {dark:?} lit {lit:?}"
        );
        let lamp = FogLight::point([0.0, 1.0, 0.0], [1.0, 0.8, 0.5], 5_000.0, 8.0);
        let near = inscatter_per_metre([0.4, 1.0, 0.0], view, &sun(0.0, 0.0), &[lamp], &fog, &[volume]);
        let far = inscatter_per_metre([3.0, 1.0, 0.0], view, &sun(0.0, 0.0), &[lamp], &fog, &[volume]);
        assert!(near[0] > far[0] * 3.0, "near {near:?} far {far:?}");
    }

    #[test]
    fn a_low_sun_is_extinguished_by_the_ground_haze_it_crosses() {
        // Mars-like ground haze: 4.2e-4 /m at the floor, 83 m scale height.
        let haze = VolumetricFogSettings {
            density: 4.2e-4,
            height_falloff: 0.012,
            base_height: 0.0,
            ..VolumetricFogSettings::default()
        };
        let toward = |deg: f32| {
            let r = deg.to_radians();
            [r.cos(), r.sin(), 0.0]
        };
        let floor = [0.0, 2.0, 0.0];
        let noon = (-fog_layer_optical_depth(floor, toward(60.0), &haze)).exp();
        let dusk = (-fog_layer_optical_depth(floor, toward(4.0), &haze)).exp();
        let up = fog_layer_optical_depth(floor, [0.0, 1.0, 0.0], &haze);
        // Closed form straight up: σ / k.
        assert!(
            (up - 4.2e-4 * (-0.012f32 * 2.0).exp() / 0.012).abs() < 1.0e-3,
            "{up}"
        );
        assert!(noon > 0.95, "a high sun barely dims, T {noon}");
        assert!(dusk < 0.65, "a 4° sun crosses ≈ 1.2 km of floor haze, T {dusk}");
        // Above the layer the sun is clear again.
        let rim = (-fog_layer_optical_depth([0.0, 400.0, 0.0], toward(4.0), &haze)).exp();
        assert!(
            rim > dusk + 0.25,
            "a mesa top sits above the haze, T {rim} vs {dusk}"
        );
        // No falloff: uniform fog over the whole range.
        let flat = VolumetricFogSettings {
            density: 1.0e-5,
            height_falloff: 0.0,
            ..VolumetricFogSettings::default()
        };
        let d = fog_layer_optical_depth([0.0; 3], toward(10.0), &flat);
        assert!((d - 1.0e-5 * FOG_LAYER_RANGE_M).abs() < 1.0e-4, "{d}");
        // The in-scatter of a valley sample follows the extinguished sun.
        let sun = |deg: f32| SunScattering {
            travel: mul(toward(deg), -1.0),
            color: [1.0; 3],
            illuminance: 20_000.0,
            shadow: 1.0,
        };
        let view = [0.0, 0.0, 1.0];
        let low = inscatter_per_metre(floor, view, &sun(4.0), &[], &haze, &[]);
        let high = inscatter_per_metre(floor, view, &sun(60.0), &[], &haze, &[]);
        assert!(low[1] < high[1] * 0.75, "low {low:?} high {high:?}");
    }

    #[test]
    fn integrate_segment_conserves_a_dark_far_end() {
        let (light, t) = integrate_segment([2.0, 2.0, 2.0], 0.2, 10.0);
        assert!(t < 0.2, "optical depth 2, transmittance {t}");
        assert!(
            light[0] > 1.0 && light[0] < 2.0 / 0.2,
            "bounded inscatter {light:?}"
        );
        let (zero, t1) = integrate_segment([0.0; 3], 0.0, 4.0);
        assert_eq!(zero, [0.0; 3]);
        assert!((t1 - 1.0).abs() < 1.0e-4);
    }

    #[test]
    fn froxel_slices_are_exponential_and_reproject() {
        let near = froxel_depth(0, 16, 0.5, 80.0);
        let far = froxel_depth(15, 16, 0.5, 80.0);
        assert!(near < 2.0, "{near}");
        assert!(far > 40.0 && far < 80.0, "{far}");
        assert!(froxel_slice_uvw(near, 0.5, 80.0) < 0.1);
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        // A point at the origin is the NDC origin only for a view-proj that maps it there.
        // This orthographic-like matrix maps world (0,0,0) to NDC 0 and z 0.5.
        let vp = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.5, 1.0],
        ];
        let uvw = reproject_froxel([0.0, 0.0, 0.0], vp, 0.5, 80.0).expect("inside");
        assert!((uvw[0] - 0.5).abs() < 1.0e-4, "{uvw:?}");
        assert!((uvw[1] - 0.5).abs() < 1.0e-4, "{uvw:?}");
        assert!((uvw[2] - 0.5).abs() < 1.0e-4, "W is clip z, {uvw:?}");
        let same = reproject_froxel([0.0, 0.0, 0.0], vp, 0.5, 8.0).expect("inside");
        assert!((same[2] - uvw[2]).abs() < 1.0e-6, "far does not change clip z");
        assert!(
            (froxel_slice_uvw(1.0, 0.5, 80.0) - froxel_slice_uvw(1.0, 0.5, 8.0)).abs() > 1.0e-3,
            "the log slice does change with far"
        );
        assert!(reproject_froxel([4.0, 0.0, 0.0], identity, 0.5, 80.0).is_none());
        let kept = blend_history([1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 0.0, 0.0], 0.5, true);
        assert!((kept[0] - 0.5).abs() < 1.0e-5);
        let rejected = blend_history([1.0, 0.0, 0.0, 1.0], [0.0; 4], 0.5, false);
        assert_eq!(rejected[0], 1.0);
    }
}
