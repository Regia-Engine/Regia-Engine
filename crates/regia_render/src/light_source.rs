// SPDX-License-Identifier: MPL-2.0
//! What an optional [`crate::LightSourceSize`] changes (ADR-1367), with CPU mirrors of the
//! shader terms so tests can check them without a GPU.
//!
//! - **Shadow:** the local shadow tile's source radius (`ShadowTile::bias.w` in
//!   `lab_forward.wgsl`) is the component's radius, or
//!   [`crate::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M`] without it ([`shadow_source_radius`]).
//!   The PCSS filter radius follows it ([`local_penumbra_texels`], a mirror of
//!   `shadows.wgsl` `shadow_pcss_atlas`). Penumbra estimation: Fernando, "Percentage-Closer
//!   Soft Shadows", SIGGRAPH 2005 sketch.
//! - **Specular:** the forward local-light loop lights the specular from a representative point
//!   of the sphere and renormalizes the GGX lobe for the wider light; the diffuse stays at the
//!   centre. The method is Lagarde & de Rousiers, "Moving Frostbite to Physically Based
//!   Rendering" v3, SIGGRAPH 2014, § 4.7.5, which uses Karis, "Real Shading in the legacy engine4",
//!   SIGGRAPH 2013 course notes, eqs. 10, 11 and 14. Regia Engine adds eq. 10's half-angle `r / 2d`
//!   to α as a variance ([`sphere_light_alpha2`]), moves the point by `1 − α/α'` of the way to
//!   the reflection ray ([`sphere_light_dir`]), and keeps the punctual tail where the ray misses
//!   the sphere ([`sphere_light_gain`]). Karis's linear form measured 4.0 % against the path
//!   tracer on the § 1.5 item 4 grid (rough metals up to 26 % dark); this form measures 0.24 %
//!   (ADR-1367). All mirror `pbr_lighting.wgsl`.
//! - **Reference:** `regia_pt` traces a point light with a positive source radius as a sphere
//!   light of the same radius and the same intensity ([`sphere_radiance_scale`]). A spot with a
//!   positive radius is a cone-limited sphere (`PtLight::SphereSpot`); a zero radius stays a point.
//!
//! A light without the component packs a specular radius of 0 (`GpuLight::cone_shadow.w`), and
//! the shader's sphere branch never runs, so such frames stay pixel-identical (ADR-041).

use regia_core::glam::Vec3;

use crate::LabLocalLight;
use crate::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M;

/// Smallest PCSS filter radius of a local-light shadow, texels (`shadow_pcss_atlas`).
pub const LOCAL_PCSS_MIN_TEXELS: f32 = 2.5;

/// Largest PCSS filter radius of a local-light shadow, texels (`shadow_pcss_atlas`).
pub const LOCAL_PCSS_MAX_TEXELS: f32 = 10.0;

/// The source radius a lab light carries, if it has one.
#[must_use]
pub fn light_source_radius(light: &LabLocalLight) -> Option<f32> {
    match light {
        LabLocalLight::Point(point) => point.source_radius,
        LabLocalLight::Spot(spot) => spot.source_radius,
    }
}

/// The radius the local shadow tile packs in `bias.w`, metres.
///
/// `None` (no component) and a non-finite value give
/// [`LOCAL_SHADOW_SOURCE_RADIUS_M`] exactly; a negative radius gives 0.
#[must_use]
pub fn shadow_source_radius(source_radius: Option<f32>) -> f32 {
    match source_radius {
        Some(radius) if radius.is_finite() => radius.max(0.0),
        _ => LOCAL_SHADOW_SOURCE_RADIUS_M,
    }
}

/// The radius the forward specular reads (`GpuLight::cone_shadow.w`), metres.
///
/// `None` and a non-finite value give 0: the punctual highlight, bit for bit.
#[must_use]
pub fn specular_source_radius(source_radius: Option<f32>) -> f32 {
    match source_radius {
        Some(radius) if radius.is_finite() => radius.max(0.0),
        _ => 0.0,
    }
}

/// Unclamped PCSS filter radius of a local-light shadow, in atlas texels.
///
/// The mirror of `sample_atlas_tile` + `shadow_pcss_atlas`: `penumbra_per_depth =
/// bias.w · bias.y / bias.x`, times the blocker gap in the tile's normalized depth. That is
/// Fernando's penumbra `(d_receiver − d_blocker) · w_light / d_blocker` in the units of the
/// perspective tile. `depth_gap` is `compare_z − average blocker depth`, `inv_depth` is
/// `bias.y` (`1/near − 1/far`) and `texel` is `bias.x` (world texel per unit view depth).
#[must_use]
pub fn local_penumbra_texels_unclamped(
    source_radius_m: f32,
    depth_gap: f32,
    inv_depth: f32,
    texel: f32,
) -> f32 {
    depth_gap * (source_radius_m * inv_depth / texel.max(1.0e-8))
}

/// PCSS filter radius of a local-light shadow, in atlas texels, clamped like the shader
/// (`LOCAL_PCSS_MIN_TEXELS..=LOCAL_PCSS_MAX_TEXELS`). A source radius of 0 still clamps up to
/// the 2.5-texel floor.
#[must_use]
pub fn local_penumbra_texels(source_radius_m: f32, depth_gap: f32, inv_depth: f32, texel: f32) -> f32 {
    local_penumbra_texels_unclamped(source_radius_m, depth_gap, inv_depth, texel)
        .clamp(LOCAL_PCSS_MIN_TEXELS, LOCAL_PCSS_MAX_TEXELS)
}

/// Eq. 10's half-angle `radius / (2 · distance)` added to the GGX α as a variance:
/// `α'² = min(α² + (r / 2d)², 1)`.
///
/// Karis adds it linearly, which over-widens a lobe wider than the source. Variance addition
/// keeps his glossy limit (`α' → r / 2d`) and leaves a rough lobe almost unchanged. The
/// specular-AA and Toksvig widenings in `pbr_lighting.wgsl` add their variances the same way.
#[must_use]
pub fn sphere_light_alpha2(alpha: f32, radius: f32, distance: f32) -> f32 {
    let w = radius / (2.0 * distance.max(1.0e-4));
    alpha.mul_add(alpha, w * w).min(1.0)
}

/// The specular light direction: from the shading point toward the sphere (centre
/// `to_center`), moved toward the reflection ray `reflection` (unit) by `1 − α/α'` of the
/// centre's offset from the ray, and never past the sphere's edge (Karis 2013 eq. 11's clamp).
///
/// For Gaussian lobes, `(α/α')²` times the lobe of width α at the moved point equals the
/// widened lobe at the centre. A glossy lobe (α ≪ r / 2d) moves almost to eq. 11's point and a
/// rough one stays at the centre.
#[must_use]
pub fn sphere_light_dir(to_center: Vec3, reflection: Vec3, radius: f32, alpha: f32) -> Vec3 {
    let center_to_ray = to_center.dot(reflection) * reflection - to_center;
    let len = center_to_ray.length().max(1.0e-6);
    let widened = sphere_light_alpha2(alpha, radius, to_center.length())
        .max(1.0e-12)
        .sqrt();
    let share = (1.0 - alpha / widened).clamp(0.0, 1.0);
    let closest = to_center + center_to_ray * share.min((radius / len).clamp(0.0, 1.0));
    closest.normalize_or_zero()
}

/// Karis 2013 eq. 14 with the widened α: the energy term `(α / α')²`. It is 1 for a radius of 0
/// and falls as the light grows or comes closer.
#[must_use]
pub fn sphere_light_normalization(alpha: f32, radius: f32, distance: f32) -> f32 {
    (alpha * alpha / sphere_light_alpha2(alpha, radius, distance).max(1.0e-12)).min(1.0)
}

/// GGX `D` (`pbr_lighting.wgsl` `d_ggx`), for [`sphere_light_gain`].
fn d_ggx(no_h: f32, alpha: f32) -> f32 {
    let a2 = alpha * alpha;
    let f = (no_h * a2 - no_h).mul_add(no_h, 1.0);
    a2 / (std::f32::consts::PI * f * f)
}

/// The factor on the specular lit from [`sphere_light_dir`]'s point: the energy term times
/// that point's cosine ratio `spec_cos`.
///
/// Where the reflection ray misses the sphere, the sphere's integral is never below the
/// punctual value at its centre, because the lobe falls off away from the ray. GGX's long tail
/// is not narrowed by a small source. So in that region the factor is at least
/// `D(centre) / D(point)`, and the tail keeps the point light's value. `no_h` and
/// `no_h_center` are N·H for the moved point and for the centre.
#[must_use]
pub fn sphere_light_gain(
    alpha: f32,
    radius: f32,
    to_center: Vec3,
    reflection: Vec3,
    no_h: f32,
    no_h_center: f32,
    spec_cos: f32,
) -> f32 {
    let gain = sphere_light_normalization(alpha, radius, to_center.length()) * spec_cos;
    let along = to_center.dot(reflection);
    if along > 0.0 && (along * reflection - to_center).length() <= radius {
        return gain;
    }
    gain.max(d_ggx(no_h_center, alpha) / d_ggx(no_h, alpha).max(1.0e-30))
}

/// Windowed inverse-square falloff of a light with a sphere source of `source_radius` metres
/// (ADR-1454), the CPU mirror of `distance_attenuation_source` in `pbr_lighting.wgsl`.
///
/// A sphere's irradiance is bounded near it (about `I / s²`, not `I / d²`), so the 1 m knee of
/// the punctual form (`d² + 1`) becomes `max(1, s²)`: a source under 1 m, and a point light,
/// keep the punctual value bit for bit, and a large source (the flash of a crash, 150 m) no
/// longer lights the ground beneath it a thousand times brighter than the ground beside it.
#[must_use]
pub fn distance_attenuation_source(distance: f32, radius: f32, source_radius: f32) -> f32 {
    let r = radius.max(1.0e-4);
    let d = distance.max(0.0);
    let window = (1.0 - (d / r).powi(4)).clamp(0.0, 1.0);
    (window * window) / (d * d + (source_radius * source_radius).max(1.0))
}

/// Radiance of a uniform sphere of `radius` metres whose intensity is 1 cd in every direction:
/// `1 / (π r²)` (a sphere's projected area is `π r²`). `None` for a radius of 0, which stays a
/// point light, and for a radius so small that the scale is not a finite `f32` (below about
/// 3·10⁻²⁰ m `π r²` underflows).
#[must_use]
pub fn sphere_radiance_scale(radius: f32) -> Option<f32> {
    if !(radius.is_finite() && radius > 0.0) {
        return None;
    }
    let scale = 1.0 / (std::f32::consts::PI * radius * radius);
    scale.is_finite().then_some(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_source_keeps_the_45_cm_shadow_bit_for_bit() {
        assert_eq!(
            shadow_source_radius(None).to_bits(),
            LOCAL_SHADOW_SOURCE_RADIUS_M.to_bits()
        );
        assert_eq!(shadow_source_radius(None).to_bits(), 0.45f32.to_bits());
        assert_eq!(
            shadow_source_radius(Some(f32::NAN)).to_bits(),
            LOCAL_SHADOW_SOURCE_RADIUS_M.to_bits()
        );
        assert_eq!(shadow_source_radius(Some(0.2)), 0.2);
        assert_eq!(shadow_source_radius(Some(-1.0)), 0.0);
        // No source, no specular widening.
        assert_eq!(specular_source_radius(None).to_bits(), 0.0f32.to_bits());
        assert_eq!(specular_source_radius(Some(f32::INFINITY)), 0.0);
        assert_eq!(specular_source_radius(Some(0.2)), 0.2);
    }

    #[test]
    fn the_penumbra_grows_linearly_with_the_source_radius() {
        // A 512² spot tile at 60° (bias.x = 2·tan(30°)/512) and near 0.05 / range 15 m.
        let texel = 2.0 * 30f32.to_radians().tan() / 512.0;
        let inv_depth = 1.0 / 0.05 - 1.0 / 15.0;
        let gap = 1.0e-4;
        let base = local_penumbra_texels_unclamped(0.1, gap, inv_depth, texel);
        assert!(base > 0.0);
        for k in [2.0f32, 3.0, 4.5, 8.0] {
            let w = local_penumbra_texels_unclamped(0.1 * k, gap, inv_depth, texel);
            assert!(
                (w / base - k).abs() < 1.0e-4,
                "radius ×{k}: penumbra ×{}",
                w / base
            );
        }
        assert_eq!(local_penumbra_texels_unclamped(0.0, gap, inv_depth, texel), 0.0);
        assert_eq!(
            local_penumbra_texels(0.0, gap, inv_depth, texel),
            LOCAL_PCSS_MIN_TEXELS
        );
        // The shader's clamp: never under the contact floor, never past the umbra cap, and
        // monotone in between.
        let mut last = 0.0;
        for i in 0..64 {
            let r = i as f32 * 0.02;
            let w = local_penumbra_texels(r, gap, inv_depth, texel);
            assert!((LOCAL_PCSS_MIN_TEXELS..=LOCAL_PCSS_MAX_TEXELS).contains(&w));
            assert!(w >= last);
            last = w;
        }
        // The default 45 cm sits where the old constant put it.
        assert_eq!(
            local_penumbra_texels(shadow_source_radius(None), gap, inv_depth, texel),
            local_penumbra_texels(0.45, gap, inv_depth, texel)
        );
    }

    #[test]
    fn a_large_source_bounds_the_near_field_and_keeps_the_far_inverse_square() {
        // Punctual form (the shader's `distance_attenuation`).
        let punctual = |d: f32, r: f32| {
            let w = (1.0 - (d / r).powi(4)).clamp(0.0, 1.0);
            w * w / (d * d + 1.0)
        };
        for d in [0.0, 0.3, 1.0, 5.0, 40.0] {
            // A source under 1 m changes nothing, bit for bit.
            assert_eq!(
                distance_attenuation_source(d, 100.0, 0.0).to_bits(),
                punctual(d, 100.0).to_bits()
            );
            assert_eq!(
                distance_attenuation_source(d, 100.0, 0.45).to_bits(),
                punctual(d, 100.0).to_bits()
            );
        }
        // A 150 m source: the value under the source is the bounded 1 / s², not 1 / 1.
        let s = 150.0f32;
        let near = distance_attenuation_source(0.0, 12_000.0, s);
        assert!((near * s * s - 1.0).abs() < 1.0e-3, "{near}");
        // Ground right under the ball (60 m) and 150 m out differ by under 2x, not 6x.
        let ratio =
            distance_attenuation_source(60.0, 12_000.0, s) / distance_attenuation_source(150.0, 12_000.0, s);
        assert!(ratio < 2.0 && ratio > 1.0, "{ratio}");
        // Beyond 10 source radii it is the inverse square within 1 %.
        for d in [1_500.0f32, 3_300.0] {
            let sphere = distance_attenuation_source(d, 12_000.0, s);
            let point = punctual(d, 12_000.0);
            assert!((sphere / point - 1.0).abs() < 0.01 + s * s / (d * d), "{d}");
        }
        // It never exceeds the punctual form and falls monotonically with distance.
        let mut last = f32::MAX;
        for i in 0..200 {
            let d = i as f32 * 20.0;
            let a = distance_attenuation_source(d, 12_000.0, s);
            assert!(a <= punctual(d, 12_000.0) && a <= last);
            last = a;
        }
    }

    #[test]
    fn a_zero_radius_sphere_is_the_point_light() {
        let to_center = Vec3::new(0.3, 2.0, -0.4);
        let reflection = Vec3::new(0.1, 0.9, 0.2).normalize();
        for alpha in [0.002025f32, 0.01, 0.25, 1.0] {
            let dir = sphere_light_dir(to_center, reflection, 0.0, alpha);
            assert!((dir - to_center.normalize()).length() < 1.0e-6);
            assert_eq!(sphere_light_normalization(alpha, 0.0, 3.0), 1.0);
            assert_eq!(sphere_light_alpha2(alpha, 0.0, 3.0), alpha * alpha);
        }
    }

    #[test]
    fn a_glossy_lobe_takes_karis_point_and_a_rough_one_stays_at_the_centre() {
        let to_center = Vec3::new(0.0, 4.0, 0.0);
        let radius = 0.5;
        // Glossy (α = 0.002 against r/2d = 0.0625): eq. 11's point on the sphere's silhouette
        // toward a missed ray, and nearly the ray itself when it goes through the sphere.
        let glossy = 0.002025f32;
        let reflection = Vec3::new(0.6, 0.8, 0.0);
        let dir = sphere_light_dir(to_center, reflection, radius, glossy);
        let center_to_ray = to_center.dot(reflection) * reflection - to_center;
        let karis = (to_center + center_to_ray * (radius / center_to_ray.length()).min(1.0)).normalize();
        assert!((dir - karis).length() < 1.0e-6, "{dir} vs eq. 11's {karis}");
        assert!(dir.angle_between(reflection) < to_center.angle_between(reflection));
        let through = Vec3::new(0.05, 1.0, 0.0).normalize();
        let hit = sphere_light_dir(to_center, through, radius, glossy);
        assert!(hit.angle_between(through) < 0.05 * to_center.angle_between(through));
        // Rough (α = 0.81): the point moves under 1 % of the way.
        let rough = sphere_light_dir(to_center, through, radius, 0.81);
        assert!(rough.angle_between(to_center) < 0.01 * through.angle_between(to_center));
        // Monotone in between: glossier lobes move further.
        let mut last = 0.0;
        for alpha in [0.81f32, 0.25, 0.09, 0.01] {
            let a = sphere_light_dir(to_center, through, radius, alpha).angle_between(to_center);
            assert!(a >= last, "α {alpha}");
            last = a;
        }
    }

    #[test]
    fn the_energy_term_is_eq_14_with_the_variance_widened_alpha() {
        let (alpha, r, d) = (0.09f32, 0.2, 4.0);
        let w = 0.2f32 / 8.0;
        assert!((sphere_light_alpha2(alpha, r, d) - (alpha * alpha + w * w)).abs() < 1.0e-8);
        let norm = sphere_light_normalization(alpha, r, d);
        assert!((norm - alpha * alpha / (alpha * alpha + w * w)).abs() < 1.0e-6);
        // Glossy lobes lose far more than rough ones; a rough lobe barely changes.
        assert!(sphere_light_normalization(0.01, r, d) < sphere_light_normalization(0.5, r, d));
        assert!(sphere_light_normalization(0.49, r, d) > 0.99);
        // Bigger or closer lights spread the highlight more; α'² saturates at 1.
        assert!(sphere_light_normalization(alpha, 0.4, d) < norm);
        assert!(sphere_light_normalization(alpha, r, 2.0) < norm);
        assert_eq!(sphere_light_alpha2(0.9, 4.0, 1.0), 1.0);
        // The glossy limit is Karis's eq. 10: α' → r / 2d.
        let glossy = sphere_light_alpha2(1.0e-4, r, d).sqrt();
        assert!((glossy / w - 1.0).abs() < 1.0e-3);
    }

    #[test]
    fn the_tail_keeps_the_punctual_value_where_the_ray_misses() {
        let to_center = Vec3::new(0.0, 4.0, 0.0);
        let (radius, alpha) = (0.2f32, 0.01f32);
        let norm = sphere_light_normalization(alpha, radius, 4.0);
        // A ray through the sphere: the energy term alone.
        let through = Vec3::new(0.01, 1.0, 0.0).normalize();
        assert_eq!(
            sphere_light_gain(alpha, radius, to_center, through, 0.9999, 0.999, 1.0),
            norm
        );
        // A ray far off the sphere: at least D(centre) / D(point), which binds here.
        let off = Vec3::new(0.5, 1.0, 0.0).normalize();
        let (no_h, no_h_center) = (0.995f32, 0.99f32);
        let gain = sphere_light_gain(alpha, radius, to_center, off, no_h, no_h_center, 1.0);
        let floor = d_ggx(no_h_center, alpha) / d_ggx(no_h, alpha);
        assert!(floor > norm, "the tail floor binds: {floor} vs {norm}");
        assert_eq!(gain, floor);
        // A sphere behind the ray is a miss too.
        let behind = Vec3::new(0.0, -1.0, 0.0);
        assert!(sphere_light_gain(alpha, radius, to_center, behind, 0.5, 0.5, 1.0) >= 1.0);
    }

    #[test]
    fn a_sphere_keeps_the_point_lights_intensity() {
        let r = 0.2;
        let scale = sphere_radiance_scale(r).expect("a sphere");
        // Intensity = radiance × projected area.
        assert!((scale * std::f32::consts::PI * r * r - 1.0).abs() < 1.0e-6);
        assert_eq!(sphere_radiance_scale(0.0), None);
        assert_eq!(sphere_radiance_scale(f32::NAN), None);
        // `π r²` underflows (its reciprocal overflows to +∞): a point light, not an infinite
        // radiance (audit render10 #12).
        assert_eq!(sphere_radiance_scale(1.0e-20), None);
        assert!(sphere_radiance_scale(1.0e-15).is_some_and(f32::is_finite));
    }

    #[test]
    fn lab_lights_report_their_source() {
        let point = LabLocalLight::Point(crate::LabPointLight {
            position: [0.0; 3],
            color: [1.0; 3],
            intensity: 800.0,
            radius: 10.0,
            cast_shadows: true,
            source_radius: Some(0.2),
        });
        assert_eq!(light_source_radius(&point), Some(0.2));
        let spot = LabLocalLight::Spot(crate::LabSpotLight {
            position: [0.0; 3],
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            intensity: 1000.0,
            range: 15.0,
            inner_angle: 0.3,
            outer_angle: 0.5,
            cast_shadows: true,
            source_radius: None,
        });
        assert_eq!(light_source_radius(&spot), None);
    }
}
