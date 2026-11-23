// SPDX-License-Identifier: MPL-2.0
//! Closed-form references for the bake's energy checks: what a probe must record from a
//! uniform emitter of a simple shape, a uniform or hemispherical sky, and a Lambertian ground
//! lit by the sun or the sky.
//!
//! A probe's `sh` holds `∫ L·Y dω` per channel on the real L1 basis (`Y00 = 1/(2√π)`,
//! `Y1·(x, y, z)`, [`regia_asset::baked_gi`]). For an emitter of uniform radiance `L` that is
//! symmetric about the axis `a` from the probe through its centre, that is
//! `L00 = Y00·L·Ω` and `L1 = Y1·L·Ω⊥·a`, with `Ω` its solid angle and `Ω⊥ = ∫ cos θ dω` its
//! projected solid angle (π × the point-to-shape form factor); [`symmetric_emitter_sh`].
//!
//! Sources (see `docs/provenance.md`): the disc, sphere and on-axis rectangle solid angles and
//! form factors are textbook radiometry (Howell, Mengüç & Siegel, "Thermal Radiation Heat
//! Transfer", configuration-factor catalogue: differential element to a parallel disc and to a
//! parallel rectangle through its corner; the rectangle solid angle as four corner pyramids);
//! a Lambertian surface of albedo ρ under irradiance E has radiance ρ·E/π.

use regia_asset::baked_gi::{SH_Y00, SH_Y1, ShRgb};
use std::f32::consts::PI;

/// Solid angle of a disc of `radius` seen from `distance` along its axis, steradians.
#[must_use]
pub fn disc_solid_angle(radius: f32, distance: f32) -> f32 {
    2.0 * PI * (1.0 - distance / (distance * distance + radius * radius).sqrt())
}

/// Projected solid angle `∫ cos θ dω` of a disc of `radius` seen from `distance` along its axis
/// (π × the form factor): `π·r² / (r² + h²)`.
#[must_use]
pub fn disc_projected_solid_angle(radius: f32, distance: f32) -> f32 {
    PI * radius * radius / (radius * radius + distance * distance)
}

/// Solid angle of a `2a × 2b` rectangle seen from `distance` along its centre normal:
/// `4·asin(a·b / √((a² + h²)(b² + h²)))`.
#[must_use]
pub fn rect_solid_angle(half_a: f32, half_b: f32, distance: f32) -> f32 {
    let (a, b, h) = (half_a, half_b, distance);
    4.0 * (a * b / ((a * a + h * h) * (b * b + h * h)).sqrt())
        .clamp(-1.0, 1.0)
        .asin()
}

/// Projected solid angle of a `2a × 2b` rectangle seen from `distance` along its centre normal,
/// from the corner form factor `F = 1/(2π)·[X/√(1+X²)·atan(Y/√(1+X²)) + Y/√(1+Y²)·atan(X/√(1+Y²))]`
/// (`X = a/h`, `Y = b/h`) of the four quarter rectangles: `Ω⊥ = 4π·F`.
#[must_use]
pub fn rect_projected_solid_angle(half_a: f32, half_b: f32, distance: f32) -> f32 {
    let x = half_a / distance;
    let y = half_b / distance;
    let sx = (1.0 + x * x).sqrt();
    let sy = (1.0 + y * y).sqrt();
    2.0 * (x / sx * (y / sx).atan() + y / sy * (x / sy).atan())
}

/// Solid angle of a sphere of `radius` whose centre is `distance` away (`distance > radius`).
#[must_use]
pub fn sphere_solid_angle(radius: f32, distance: f32) -> f32 {
    let s = (radius / distance).clamp(0.0, 1.0);
    2.0 * PI * (1.0 - (1.0 - s * s).sqrt())
}

/// Projected solid angle of a sphere seen from `distance` toward its centre: `π·(r/d)²`.
#[must_use]
pub fn sphere_projected_solid_angle(radius: f32, distance: f32) -> f32 {
    let s = (radius / distance).clamp(0.0, 1.0);
    PI * s * s
}

/// The SH a probe records from a uniform emitter of `radiance` symmetric about the unit `axis`
/// (from the probe toward its centre): `L00 = Y00·L·Ω`, `L1 = Y1·L·Ω⊥·axis`.
#[must_use]
pub fn symmetric_emitter_sh(
    radiance: [f32; 3],
    solid_angle: f32,
    projected_solid_angle: f32,
    axis: [f32; 3],
) -> ShRgb {
    let mut out = [[0.0f32; 4]; 3];
    for (row, l) in out.iter_mut().zip(radiance) {
        let first = SH_Y1 * l * projected_solid_angle;
        *row = [
            SH_Y00 * l * solid_angle,
            first * axis[0],
            first * axis[1],
            first * axis[2],
        ];
    }
    out
}

/// A uniform environment of `radiance` (`L00 = 4π·Y00·L`, no L1).
#[must_use]
pub fn uniform_sh(radiance: [f32; 3]) -> ShRgb {
    symmetric_emitter_sh(radiance, 4.0 * PI, 0.0, [0.0, 1.0, 0.0])
}

/// A uniform hemisphere of `radiance` around the unit `axis` (`Ω = 2π`, `Ω⊥ = π`).
#[must_use]
pub fn hemisphere_sh(radiance: [f32; 3], axis: [f32; 3]) -> ShRgb {
    symmetric_emitter_sh(radiance, 2.0 * PI, PI, axis)
}

/// Sum of two SH sets.
#[must_use]
pub fn add_sh(a: &ShRgb, b: &ShRgb) -> ShRgb {
    let mut out = *a;
    for (o, v) in out.iter_mut().flatten().zip(b.iter().flatten()) {
        *o += v;
    }
    out
}

/// Radiance of a Lambertian surface of `albedo` under `irradiance` (lux): `ρ·E/π`.
#[must_use]
pub fn lambert_radiance(albedo: [f32; 3], irradiance: [f32; 3]) -> [f32; 3] {
    [
        albedo[0] * irradiance[0] / PI,
        albedo[1] * irradiance[1] / PI,
        albedo[2] * irradiance[2] / PI,
    ]
}

/// Luminous flux (lm) a Lambertian emitter of `luminance` (cd/m²) and `area` (m²) emits,
/// `π·L·A` per emitting side.
#[must_use]
pub fn lambert_emitter_power(luminance: f32, area: f32, two_sided: bool) -> f32 {
    let sides = if two_sided { 2.0 } else { 1.0 };
    PI * luminance * area * sides
}

/// The luminance (cd/m²) a Lambertian emitter of `area` needs to emit `power` lm
/// (the inverse of [`lambert_emitter_power`]).
#[must_use]
pub fn lambert_emitter_luminance(power: f32, area: f32, two_sided: bool) -> f32 {
    let sides = if two_sided { 2.0 } else { 1.0 };
    power / (PI * area * sides)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(∫ dω, ∫ cos θ dω)` over a planar shape `inside(x, y)` in the plane at `distance` facing
    /// the origin, by midpoint quadrature over `[-extent, extent]²` (`dω = h dA / r³`).
    fn quadrature(extent: f32, distance: f32, inside: impl Fn(f32, f32) -> bool) -> (f64, f64) {
        let n = 1200;
        let step = 2.0 * f64::from(extent) / f64::from(n);
        let h = f64::from(distance);
        let (mut omega, mut proj) = (0.0f64, 0.0f64);
        for j in 0..n {
            for i in 0..n {
                let x = -f64::from(extent) + (f64::from(i) + 0.5) * step;
                let y = -f64::from(extent) + (f64::from(j) + 0.5) * step;
                if !inside(x as f32, y as f32) {
                    continue;
                }
                let r2 = x * x + y * y + h * h;
                let cos = h / r2.sqrt();
                let d_omega = cos * step * step / r2;
                omega += d_omega;
                proj += d_omega * cos;
            }
        }
        (omega, proj)
    }

    #[test]
    fn disc_formulas_match_quadrature() {
        for (r, h) in [(0.5f32, 2.0f32), (1.0, 1.0), (3.0, 0.5)] {
            let (o, p) = quadrature(r, h, |x, y| x * x + y * y <= r * r);
            assert!(
                (disc_solid_angle(r, h) as f64 - o).abs() < 5e-3 * o,
                "{r} {h}: {o}"
            );
            assert!(
                (disc_projected_solid_angle(r, h) as f64 - p).abs() < 5e-3 * p,
                "{r} {h}: {p}"
            );
        }
        // An infinite disc is the hemisphere.
        assert!((disc_solid_angle(1e6, 1.0) - 2.0 * PI).abs() < 1e-3);
        assert!((disc_projected_solid_angle(1e6, 1.0) - PI).abs() < 1e-3);
    }

    #[test]
    fn rectangle_formulas_match_quadrature() {
        for (a, b, h) in [(0.5f32, 0.25f32, 1.0f32), (1.0, 2.0, 0.7), (0.1, 0.1, 3.0)] {
            let (o, p) = quadrature(a.max(b), h, |x, y| x.abs() <= a && y.abs() <= b);
            assert!(
                (rect_solid_angle(a, b, h) as f64 - o).abs() < 5e-3 * o,
                "{a} {b} {h}: {o}"
            );
            assert!(
                (rect_projected_solid_angle(a, b, h) as f64 - p).abs() < 5e-3 * p,
                "{a} {b} {h}: {p}"
            );
        }
        assert!((rect_solid_angle(1e6, 1e6, 1.0) - 2.0 * PI).abs() < 1e-3);
        assert!((rect_projected_solid_angle(1e6, 1e6, 1.0) - PI).abs() < 1e-3);
    }

    #[test]
    fn sphere_formulas_match_a_cone() {
        // A sphere subtends the cone of half-angle asin(r/d): the cap formulas.
        let (r, d) = (1.0f32, 3.0f32);
        let half = (r / d).asin();
        assert!((sphere_solid_angle(r, d) - 2.0 * PI * (1.0 - half.cos())).abs() < 1e-6);
        assert!((sphere_projected_solid_angle(r, d) - PI * half.sin().powi(2)).abs() < 1e-6);
    }

    #[test]
    fn emitter_power_and_luminance_invert() {
        let l = lambert_emitter_luminance(1000.0, 0.5, false);
        assert!((l - 1000.0 / (0.5 * PI)).abs() < 1e-3);
        assert!((lambert_emitter_power(l, 0.5, false) - 1000.0).abs() < 1e-2);
        assert!((lambert_emitter_power(l, 0.5, true) - 2000.0).abs() < 1e-2);
        let sh = hemisphere_sh([1.0; 3], [0.0, 1.0, 0.0]);
        let both = add_sh(&sh, &hemisphere_sh([1.0; 3], [0.0, -1.0, 0.0]));
        assert!((both[0][0] - uniform_sh([1.0; 3])[0][0]).abs() < 1e-5);
        assert!(both[0][2].abs() < 1e-6);
    }
}
