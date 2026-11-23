// SPDX-License-Identifier: MPL-2.0
//! An analytic ground plane for the image-based light (ADR-2000).
//!
//! An HDR image is a whole sphere of light, so an object lit by one alone takes the image's lower
//! half on its underside and far sides, where in a real set the floor stands. This module holds
//! the plane the environment light should see instead: a horizontal disc (or an unbounded plane)
//! at a height, of an albedo. The forward shader (`forward_env_ground.wgsl`, mirrored here)
//! replaces, on every surface above it, the part of the image the plane hides with the plane's own
//! radiance, in the diffuse term and in the reflection. Only the image light changes: the sun and
//! the local lights are left as they are, and nothing is drawn (a plane the camera should see is a
//! mesh of the scene).
//!
//! The diffuse share is the form factor of the spherical cap the plane fills, horizon-clipped in
//! closed form (Lagarde & de Rousiers 2014, "Moving Frostbite to PBR", § 4.7.2, the sphere/disk
//! illuminance after Snyder 1996). A disc's cap is exact in the plane through the point and the
//! disc's centre (its near and far edges are the cap's two ends), and round across it.

/// The plane the image light sees below an object (`FrameRenderer::set_environment_ground`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvGroundPlane {
    /// World height (Y) of the plane.
    pub height: f32,
    /// Radius of the disc around [`Self::center`], meters; `None` is an unbounded plane.
    pub radius: Option<f32>,
    /// World XZ of the disc's centre.
    pub center: [f32; 2],
    /// Diffuse albedo of the plane, linear RGB 0..1.
    pub albedo: [f32; 3],
    /// The plane also shows the frame's sun (it is lit by it, unshadowed). The sun's direct light
    /// on the object is never touched.
    pub sun_bounce: bool,
}

impl Default for EnvGroundPlane {
    fn default() -> Self {
        Self {
            height: 0.0,
            radius: None,
            center: [0.0, 0.0],
            albedo: [0.18, 0.18, 0.18],
            sun_bounce: true,
        }
    }
}

/// Why an [`EnvGroundPlane`] was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EnvGroundError {
    /// The height or centre is not finite.
    #[error("the plane's height and center must be finite")]
    BadPlace,
    /// The radius is not finite or not above 0.
    #[error("radius must be finite and > 0 (omit it for an unbounded plane)")]
    BadRadius,
    /// An albedo channel is outside 0..1.
    #[error("albedo must be within 0 to 1 in every channel")]
    BadAlbedo,
}

impl EnvGroundPlane {
    /// Checks the plane's values.
    ///
    /// # Errors
    /// [`EnvGroundError`] names the first bad value.
    pub fn validate(&self) -> Result<(), EnvGroundError> {
        if !(self.height.is_finite() && self.center.iter().all(|c| c.is_finite())) {
            return Err(EnvGroundError::BadPlace);
        }
        if self.radius.is_some_and(|r| !(r.is_finite() && r > 0.0)) {
            return Err(EnvGroundError::BadRadius);
        }
        if !self.albedo.iter().all(|c| (0.0..=1.0).contains(c)) {
            return Err(EnvGroundError::BadAlbedo);
        }
        Ok(())
    }

    /// The `env_ground` uniform: height, radius (0 = unbounded), centre x, z.
    pub(crate) fn uniform(&self) -> [f32; 4] {
        [
            self.height,
            self.radius.unwrap_or(0.0),
            self.center[0],
            self.center[1],
        ]
    }

    /// Radiance the plane sends up (cd/m²): its albedo over `plane_feed` (irradiance / π of a
    /// white plane under the image's upper half) plus, with [`Self::sun_bounce`], the sun's
    /// `sun_lux` (linear RGB lux on a surface facing it) at `sun_up` (the cosine toward +Y).
    #[must_use]
    pub fn radiance(&self, plane_feed: [f32; 3], sun_lux: [f32; 3], sun_up: f32) -> [f32; 3] {
        let sun = if self.sun_bounce { sun_up.max(0.0) } else { 0.0 };
        [0, 1, 2].map(|c| self.albedo[c] * (plane_feed[c] + sun_lux[c] * sun * std::f32::consts::FRAC_1_PI))
    }
}

/// Fraction (0..1) of a cosine-weighted hemisphere filled by a spherical cap of angular radius σ
/// whose axis makes `cos_theta` with the normal; `sin_sigma_sq` is sin²σ. Mirrors
/// `env_cap_form_factor`.
#[must_use]
pub fn cap_form_factor(cos_theta: f32, sin_sigma_sq: f32) -> f32 {
    let s2 = sin_sigma_sq.clamp(1e-6, 1.0);
    if cos_theta * cos_theta > s2 {
        return s2 * cos_theta.clamp(0.0, 1.0);
    }
    let sin_theta = (1.0 - cos_theta * cos_theta).max(0.0).sqrt().max(1e-5);
    let x = (1.0 / s2 - 1.0).max(0.0).sqrt();
    let y = (-x * cos_theta / sin_theta).clamp(-1.0, 1.0);
    let w = sin_theta * (1.0 - y * y).max(0.0).sqrt();
    let e = (cos_theta * y.acos() - x * w) * s2 + w.atan2(x);
    (e / std::f32::consts::PI).clamp(0.0, 1.0)
}

/// What a plane covers of a surface's view. Mirrors `env_ground_cover`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaneCover {
    /// Share of the surface's cosine-weighted hemisphere the plane fills.
    pub form_factor: f32,
    /// Share of the image's lower-hemisphere light the plane hides from the surface.
    pub lower_share: f32,
}

/// [`PlaneCover`] of the surface at `p` with unit normal `n`.
#[must_use]
pub fn plane_cover(plane: &EnvGroundPlane, p: [f32; 3], n: [f32; 3]) -> PlaneCover {
    let [_, radius, cx, cz] = plane.uniform();
    let h = p[1] - plane.height;
    if h < -0.01 {
        return PlaneCover {
            form_factor: 0.0,
            lower_share: 0.0,
        };
    }
    let h = h.max(1e-3);
    let lower = 0.5 * (1.0 - n[1]);
    let (cos_theta, sin_sigma_sq) = if radius <= 0.0 {
        (-n[1], 1.0)
    } else {
        let (dx, dz) = (cx - p[0], cz - p[2]);
        let offset = (dx * dx + dz * dz).sqrt();
        // The cap's two ends are the disc's near and far edges seen from the point.
        let near = ((radius - offset) / h).atan();
        let far = ((radius + offset) / h).atan();
        let half = 0.5 * (near + far);
        let tilt = 0.5 * (far - near);
        let (ux, uz) = if offset > 1e-6 {
            (dx / offset, dz / offset)
        } else {
            (0.0, 0.0)
        };
        let axis = [tilt.sin() * ux, -tilt.cos(), tilt.sin() * uz];
        // Past the edge the disc is a thin ellipse wider than its in-plane cap: its projected
        // area sets the cap instead (exact straight above the centre).
        let distance_sq = offset * offset + h * h;
        let projected = radius * radius * h / distance_sq.sqrt();
        (
            n[0] * axis[0] + n[1] * axis[1] + n[2] * axis[2],
            half.sin().powi(2).max(projected / (projected + distance_sq)),
        )
    };
    let form_factor = cap_form_factor(cos_theta, sin_sigma_sq);
    PlaneCover {
        form_factor,
        lower_share: (form_factor / lower.max(1e-4)).clamp(0.0, 1.0),
    }
}

/// The share of a reflection of unit direction `r` from `p`, of perceptual roughness `perceptual`,
/// that lands on the plane. Mirrors `env_ground_reflection`.
#[must_use]
pub fn reflection_share(plane: &EnvGroundPlane, p: [f32; 3], r: [f32; 3], perceptual: f32) -> f32 {
    let [_, radius, cx, cz] = plane.uniform();
    let h = p[1] - plane.height;
    if h < -0.01 {
        return 0.0;
    }
    let spread = 0.01 + perceptual * perceptual;
    let below = smoothstep(spread, -spread, r[1]);
    if radius <= 0.0 {
        return below;
    }
    let down = r[1].min(-0.5 * spread);
    let t = h.max(0.0) / -down;
    let (x, z) = (p[0] + r[0] * t - cx, p[2] + r[2] * t - cz);
    let edge = 0.02 + perceptual;
    below
        * (1.0
            - smoothstep(
                radius * (1.0 - edge),
                radius * (1.0 + edge),
                (x * x + z * z).sqrt(),
            ))
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize(v: [f32; 3]) -> [f32; 3] {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        v.map(|c| c / l)
    }

    /// The share of a cosine-weighted hemisphere around `n` whose rays from `p` land on the disc,
    /// by a fixed stratified sum over the hemisphere.
    fn monte_carlo(plane: &EnvGroundPlane, p: [f32; 3], n: [f32; 3]) -> f32 {
        let t = normalize(if n[0].abs() > 0.5 {
            [n[2], 0.0, -n[0]]
        } else {
            [0.0, -n[2], n[1]]
        });
        let b = [
            n[1] * t[2] - n[2] * t[1],
            n[2] * t[0] - n[0] * t[2],
            n[0] * t[1] - n[1] * t[0],
        ];
        let steps = 400;
        let mut hit = 0u32;
        for i in 0..steps {
            for j in 0..steps {
                let u = (i as f32 + 0.5) / steps as f32;
                let v = (j as f32 + 0.5) / steps as f32;
                let (s, c) = (std::f32::consts::TAU * v).sin_cos();
                let r = u.sqrt();
                let z = (1.0 - u).sqrt();
                let d = [0, 1, 2].map(|k| t[k] * r * c + b[k] * r * s + n[k] * z);
                if d[1] >= 0.0 {
                    continue;
                }
                let step = (p[1] - plane.height) / -d[1];
                let (x, y) = (
                    p[0] + d[0] * step - plane.center[0],
                    p[2] + d[2] * step - plane.center[1],
                );
                if plane.radius.is_none_or(|radius| x * x + y * y <= radius * radius) {
                    hit += 1;
                }
            }
        }
        hit as f32 / (steps * steps) as f32
    }

    #[test]
    fn an_unbounded_plane_fills_exactly_the_lower_share() {
        let plane = EnvGroundPlane::default();
        for n in [
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            normalize([0.3, -0.6, 0.2]),
        ] {
            let c = plane_cover(&plane, [0.0, 2.0, 0.0], n);
            let want = 0.5 * (1.0 - n[1]);
            assert!((c.form_factor - want).abs() < 1e-3, "{n:?}: {}", c.form_factor);
            if want > 1e-3 {
                assert!((c.lower_share - 1.0).abs() < 2e-3, "{n:?}: {}", c.lower_share);
            }
        }
    }

    #[test]
    fn the_cap_form_factor_matches_its_limits() {
        // Fully above the horizon: sin²σ · cos θ.
        assert!((cap_form_factor(1.0, 0.25) - 0.25).abs() < 1e-5);
        assert!((cap_form_factor(0.9, 0.25) - 0.225).abs() < 1e-5);
        // Fully below it.
        assert_eq!(cap_form_factor(-0.9, 0.25), 0.0);
        // A cap straddling the horizon is between 0 and the unclipped value.
        let straddling = cap_form_factor(0.2, 0.5);
        assert!(straddling > 0.1 && straddling < 0.5, "{straddling}");
    }

    #[test]
    fn a_disc_tracks_a_numeric_integral_within_five_percent_of_the_hemisphere() {
        let plane = EnvGroundPlane {
            height: 0.0,
            radius: Some(3.0),
            ..EnvGroundPlane::default()
        };
        let mut worst = 0.0f32;
        for (p, n) in [
            ([0.0, 1.0, 0.0], [0.0, -1.0, 0.0]),
            ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
            ([0.0, 0.2, 0.0], normalize([1.0, -1.0, 0.0])),
            ([2.0, 1.0, 0.0], [0.0, -1.0, 0.0]),
            ([2.0, 1.0, 0.0], [-1.0, 0.0, 0.0]),
            ([2.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
            ([0.0, 6.0, 0.0], normalize([1.0, -0.5, 0.0])),
            ([5.0, 1.0, 0.0], normalize([-1.0, -1.0, 0.0])),
        ] {
            let got = plane_cover(&plane, p, n).form_factor;
            let want = monte_carlo(&plane, p, n);
            eprintln!("{p:?} {n:?}: analytic {got:.3} numeric {want:.3}");
            worst = worst.max((got - want).abs());
        }
        assert!(worst < 0.05, "worst error {worst}");
    }

    #[test]
    fn a_surface_below_the_plane_sees_none_of_it() {
        let plane = EnvGroundPlane {
            height: 5.0,
            ..EnvGroundPlane::default()
        };
        let c = plane_cover(&plane, [0.0, 1.0, 0.0], [0.0, -1.0, 0.0]);
        assert_eq!(c.form_factor, 0.0);
        assert_eq!(
            reflection_share(&plane, [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], 0.0),
            0.0
        );
    }

    #[test]
    fn a_reflection_lands_on_the_plane_below_the_horizon_and_inside_the_disc() {
        let unbounded = EnvGroundPlane::default();
        let p = [0.0, 1.0, 0.0];
        assert_eq!(reflection_share(&unbounded, p, [0.0, -1.0, 0.0], 0.0), 1.0);
        assert_eq!(reflection_share(&unbounded, p, [0.0, 1.0, 0.0], 0.0), 0.0);
        let disc = EnvGroundPlane {
            radius: Some(2.0),
            ..unbounded
        };
        assert_eq!(reflection_share(&disc, p, [0.0, -1.0, 0.0], 0.0), 1.0);
        // 45° down from 1 m lands 1 m out: inside. Nearly level, it lands past the edge.
        assert_eq!(reflection_share(&disc, p, normalize([1.0, -1.0, 0.0]), 0.0), 1.0);
        assert_eq!(reflection_share(&disc, p, normalize([1.0, -0.1, 0.0]), 0.0), 0.0);
        // A rough lobe straddling the horizon goes partly to the plane.
        let share = reflection_share(&unbounded, p, normalize([1.0, -0.02, 0.0]), 0.5);
        assert!(share > 0.4 && share < 0.8, "{share}");
    }

    #[test]
    fn the_plane_radiance_follows_albedo_image_and_sun() {
        let plane = EnvGroundPlane::default();
        let r = plane.radiance([1000.0; 3], [100_000.0 * std::f32::consts::PI; 3], 0.5);
        assert!((r[0] - 0.18 * (1000.0 + 50_000.0)).abs() < 1.0, "{r:?}");
        let quiet = EnvGroundPlane {
            sun_bounce: false,
            ..plane
        }
        .radiance([1000.0; 3], [100_000.0; 3], 0.5);
        assert!((quiet[1] - 180.0).abs() < 1e-3, "{quiet:?}");
        // A sun below the horizon adds nothing.
        assert_eq!(plane.radiance([0.0; 3], [100_000.0; 3], -0.5), [0.0; 3]);
    }

    #[test]
    fn validation_refuses_bad_values() {
        let good = EnvGroundPlane::default();
        assert!(good.validate().is_ok());
        for (bad, err) in [
            (
                EnvGroundPlane {
                    height: f32::NAN,
                    ..good
                },
                EnvGroundError::BadPlace,
            ),
            (
                EnvGroundPlane {
                    radius: Some(0.0),
                    ..good
                },
                EnvGroundError::BadRadius,
            ),
            (
                EnvGroundPlane {
                    albedo: [0.5, 1.5, 0.5],
                    ..good
                },
                EnvGroundError::BadAlbedo,
            ),
        ] {
            assert_eq!(bad.validate(), Err(err));
        }
    }
}
