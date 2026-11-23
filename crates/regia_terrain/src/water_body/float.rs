// SPDX-License-Identifier: MPL-2.0
//! Floating bodies (spec 87 § 5, ADR-1828): buoyancy and drag from the water surface.
//!
//! A [`Floater`] is a set of spheres fixed in a body's frame. Each sphere's submerged part is the
//! spherical cap under the water surface over its center; Archimedes' principle gives the buoyant
//! force `ρ g V` straight up at the sphere, and the offset from the center of mass turns it into a
//! righting torque. Drag is linear in the velocity relative to the water (orbital motion plus drift
//! or flow), in proportion to the submerged share. [`float_forces`] is pure; the physics bridge
//! (`regia_framework::buoyancy`) applies its result as the body's external force each fixed step.

use glam::{Quat, Vec3};
use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use super::{WaterBodyError, WaterSurface};

/// Most spheres one floater carries.
pub const MAX_FLOAT_POINTS: usize = 64;

/// Density of fresh water (kg/m³), the default [`Floater::fluid_density`].
pub const FRESH_WATER_DENSITY: f32 = 1000.0;

/// Density of sea water (kg/m³).
pub const SEA_WATER_DENSITY: f32 = 1025.0;

/// One sphere of a [`Floater`], in the body's frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FloatPoint {
    /// Center in the body's local frame (m).
    pub offset: [f32; 3],
    /// Radius (m).
    pub radius: f32,
}

/// What makes a body float: its spheres, the fluid density and the drag (spec 87 § 5.1).
#[derive(Component, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Floater {
    /// The spheres, at most [`MAX_FLOAT_POINTS`].
    pub points: Vec<FloatPoint>,
    /// Density of the water (kg/m³).
    #[serde(default = "fresh")]
    pub fluid_density: f32,
    /// Linear drag at full submersion (N·s/m), against the velocity relative to the water.
    #[serde(default)]
    pub linear_drag: f32,
    /// Angular drag at full submersion (N·m·s).
    #[serde(default)]
    pub angular_drag: f32,
}

fn fresh() -> f32 {
    FRESH_WATER_DENSITY
}

impl Floater {
    /// One sphere of radius `radius` at the body's origin, no drag.
    #[must_use]
    pub fn sphere(radius: f32) -> Self {
        Self {
            points: vec![FloatPoint {
                offset: [0.0; 3],
                radius,
            }],
            fluid_density: FRESH_WATER_DENSITY,
            linear_drag: 0.0,
            angular_drag: 0.0,
        }
    }

    /// A box of `half_extents` centered on the body's origin, filled with `counts` spheres per
    /// axis, one per cell, each with the volume of its cell, so the spheres' volume is the box's.
    /// Cells near cubic give the closest draft. The product of the counts is reduced only so it
    /// stays within 4096. [`Self::validate`] rejects more than [`MAX_FLOAT_POINTS`] spheres, and
    /// [`float_forces`] applies only the first [`MAX_FLOAT_POINTS`].
    #[must_use]
    pub fn box_points(half_extents: [f32; 3], counts: [u32; 3]) -> Self {
        let mut n = counts.map(|c| c.max(1) as usize);
        // Bound the allocation: counts above `MAX_FLOAT_POINTS` still build (so `validate` can
        // refuse them) but never past a hard cap.
        const HARD_CAP: usize = 4096;
        while n[0].saturating_mul(n[1]).saturating_mul(n[2]) > HARD_CAP {
            let axis = (0..3).max_by_key(|&a| n[a]).unwrap_or(0);
            n[axis] = (n[axis] / 2).max(1);
        }
        let cell = [
            2.0 * half_extents[0] / n[0] as f32,
            2.0 * half_extents[1] / n[1] as f32,
            2.0 * half_extents[2] / n[2] as f32,
        ];
        let radius = (3.0 * cell[0] * cell[1] * cell[2] / (4.0 * std::f32::consts::PI)).cbrt();
        let mut points = Vec::with_capacity(n[0] * n[1] * n[2]);
        for k in 0..n[2] {
            for j in 0..n[1] {
                for i in 0..n[0] {
                    points.push(FloatPoint {
                        offset: [
                            -half_extents[0] + (i as f32 + 0.5) * cell[0],
                            -half_extents[1] + (j as f32 + 0.5) * cell[1],
                            -half_extents[2] + (k as f32 + 0.5) * cell[2],
                        ],
                        radius,
                    });
                }
            }
        }
        Self {
            points,
            fluid_density: FRESH_WATER_DENSITY,
            linear_drag: 0.0,
            angular_drag: 0.0,
        }
    }

    /// The spheres' total volume (m³).
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.points.iter().map(|p| sphere_volume(p.radius)).sum()
    }

    /// Checks the points, the density and the drag.
    ///
    /// # Errors
    ///
    /// The first problem found.
    pub fn validate(&self) -> Result<(), WaterBodyError> {
        if self.points.is_empty() || self.points.len() > MAX_FLOAT_POINTS {
            return Err(WaterBodyError::OutOfRange {
                name: "floater.points",
                value: self.points.len() as f32,
                min: 1.0,
                max: MAX_FLOAT_POINTS as f32,
            });
        }
        for p in &self.points {
            if !p.offset.iter().all(|c| c.is_finite()) {
                return Err(WaterBodyError::NotFinite("floater.offset"));
            }
            check("floater.radius", p.radius, 1.0e-3, 100.0)?;
        }
        check("floater.fluid_density", self.fluid_density, 1.0, 20_000.0)?;
        check("floater.linear_drag", self.linear_drag, 0.0, 1.0e7)?;
        check("floater.angular_drag", self.angular_drag, 0.0, 1.0e7)
    }
}

fn check(name: &'static str, value: f32, min: f32, max: f32) -> Result<(), WaterBodyError> {
    if !value.is_finite() {
        return Err(WaterBodyError::NotFinite(name));
    }
    if value < min || value > max {
        return Err(WaterBodyError::OutOfRange {
            name,
            value,
            min,
            max,
        });
    }
    Ok(())
}

/// Volume of a sphere (m³).
#[must_use]
pub fn sphere_volume(radius: f32) -> f32 {
    4.0 / 3.0 * std::f32::consts::PI * radius * radius * radius
}

/// Volume (m³) of the part of a sphere of radius `radius` below a level `submerged` meters above
/// its lowest point: the spherical cap `π s² (3r − s) / 3`, `s` clamped to `0..=2r`.
#[must_use]
pub fn submerged_cap(radius: f32, submerged: f32) -> f32 {
    let s = submerged.clamp(0.0, 2.0 * radius);
    std::f32::consts::PI * s * s * (3.0 * radius - s) / 3.0
}

/// Where a floating body is and how it moves, in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatPose {
    /// The body's origin (the frame the points are in).
    pub position: Vec3,
    /// The body's rotation.
    pub rotation: Quat,
    /// The center of mass (torques are taken about it).
    pub center_of_mass: Vec3,
    /// Linear velocity of the center of mass (m/s).
    pub linear_velocity: Vec3,
    /// Angular velocity (rad/s).
    pub angular_velocity: Vec3,
}

/// The water's push on a floater ([`float_forces`]).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct FloatForces {
    /// Net force (N), world space: buoyancy plus drag.
    pub force: [f32; 3],
    /// Net torque about the center of mass (N·m).
    pub torque: [f32; 3],
    /// Submerged volume (m³).
    pub submerged_volume: f32,
    /// Submerged share of the spheres this step queries (at most [`MAX_FLOAT_POINTS`]), 0..=1.
    pub submerged_fraction: f32,
    /// Spheres touching the water.
    pub wet_points: u32,
}

/// The buoyant and drag force and torque on `floater` at `pose`, with `water(x, z)` the surface
/// over a point (`None`: no water there) and `gravity` the downward acceleration (m/s²).
///
/// Each sphere is judged by the surface over its center: the cap under that height is
/// submerged. The buoyancy of a sphere acts at its center (small spheres keep the error of not
/// using the cap's centroid small). A sample whose choppy-wave inverse missed the sphere's center
/// by more than half its radius ([`WaterSurface::miss`], a folded crest) gives the height of a
/// distant rest point, so that sphere gets no force this step instead of a wrong one (audit
/// wave 5, render M3). At most the first [`MAX_FLOAT_POINTS`] spheres are queried; a longer
/// list fails [`Floater::validate`] and the extras add no force.
#[must_use]
pub fn float_forces(
    floater: &Floater,
    pose: &FloatPose,
    water: impl Fn(f32, f32) -> Option<WaterSurface>,
    gravity: f32,
) -> FloatForces {
    // Drag and the submerged share use the spheres this step queries: points past the cap add
    // no force and do not dilute the share. `volume()` sums every radius, and a NaN or a negative
    // cube next to a real sphere collapses that sum (the drag scale `volume / total` then blows
    // up); the loop below skips those radii, so the divisor skips them too.
    let total = floater
        .points
        .iter()
        .take(MAX_FLOAT_POINTS)
        .filter(|p| p.radius.is_finite() && p.radius > 0.0)
        .map(|p| sphere_volume(p.radius))
        .sum::<f32>()
        .max(1.0e-12);
    let mut force = Vec3::ZERO;
    let mut torque = Vec3::ZERO;
    let mut submerged = 0.0f32;
    let mut wet = 0u32;
    for point in floater.points.iter().take(MAX_FLOAT_POINTS) {
        let p = pose.position + pose.rotation * Vec3::from_array(point.offset);
        let Some(surface) = water(p.x, p.z) else {
            continue;
        };
        let r = point.radius;
        // `validate` rejects these, but this runs on hand-built and deserialized floaters too.
        if !(r.is_finite() && r > 0.0) {
            continue;
        }
        if surface.miss.is_nan() || surface.miss > 0.5 * r {
            continue;
        }
        let volume = submerged_cap(r, surface.height - (p.y - r));
        if volume.is_nan() || volume <= 0.0 {
            continue;
        }
        wet += 1;
        submerged += volume;
        let arm = p - pose.center_of_mass;
        let point_velocity = pose.linear_velocity + pose.angular_velocity.cross(arm);
        let relative = point_velocity - Vec3::from_array(surface.velocity);
        let f = Vec3::new(0.0, floater.fluid_density * gravity * volume, 0.0)
            - floater.linear_drag * (volume / total) * relative;
        force += f;
        torque += arm.cross(f);
    }
    torque -= floater.angular_drag * (submerged / total) * pose.angular_velocity;
    FloatForces {
        force: force.to_array(),
        torque: torque.to_array(),
        submerged_volume: submerged,
        submerged_fraction: (submerged / total).clamp(0.0, 1.0),
        wet_points: wet,
    }
}

/// The last fixed step's [`FloatForces`] of a floating body, written by the physics bridge
/// (agent `water.floaters`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct FloaterState {
    /// The forces of the last step.
    pub forces: FloatForces,
    /// The water clock tick they were computed at.
    pub tick: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::water_body::{RibbonPoint, WaterBody, WaterClock, WaterLook};

    const G: f32 = 9.81;

    fn level(y: f32) -> impl Fn(f32, f32) -> Option<WaterSurface> {
        let lake = WaterBody::plane([0.0, y, 0.0], [100.0, 100.0], WaterLook::lake());
        move |x, z| lake.surface(x, z, &WaterClock::default())
    }

    fn at(y: f32) -> FloatPose {
        FloatPose {
            position: Vec3::new(1.0, y, -2.0),
            rotation: Quat::IDENTITY,
            center_of_mass: Vec3::new(1.0, y, -2.0),
            linear_velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
        }
    }

    /// Equilibrium height of the body's origin for a mass `mass`: bisection on the net vertical
    /// force (buoyancy minus weight).
    fn equilibrium(f: &Floater, mass: f32, rotation: Quat) -> f32 {
        let water = level(0.0);
        let (mut lo, mut hi) = (-10.0f32, 10.0f32);
        for _ in 0..60 {
            let m = 0.5 * (lo + hi);
            let pose = FloatPose { rotation, ..at(m) };
            let up = float_forces(f, &pose, &water, G).force[1] - mass * G;
            if up > 0.0 { lo = m } else { hi = m }
        }
        0.5 * (lo + hi)
    }

    /// A sphere of half the water's density floats with its center on the level (the cap is a
    /// hemisphere); fully under it carries ρ g V; above it nothing.
    #[test]
    fn sphere_of_half_density_floats_at_its_center() {
        let f = Floater::sphere(0.5);
        let mass = 0.5 * FRESH_WATER_DENSITY * sphere_volume(0.5);
        let y = equilibrium(&f, mass, Quat::IDENTITY);
        assert!(y.abs() < 1.0e-4, "center at {y}");
        let deep = float_forces(&f, &at(-3.0), level(0.0), G);
        assert!((deep.force[1] - FRESH_WATER_DENSITY * G * sphere_volume(0.5)).abs() < 1.0e-2);
        assert_eq!(deep.submerged_fraction, 1.0);
        let dry = float_forces(&f, &at(3.0), level(0.0), G);
        assert_eq!(dry, FloatForces::default());
        // No water at all: nothing.
        assert_eq!(
            float_forces(&f, &at(-3.0), |_, _| None, G),
            FloatForces::default()
        );
    }

    /// Audit wave 5 (render M3): a sphere whose surface sample is a failed choppy inverse (the
    /// miss is past half its radius, or not a number) gets no force; a sample within that bound
    /// still floats it as before.
    #[test]
    fn a_failed_wave_inverse_applies_no_buoyancy() {
        let f = Floater::sphere(0.5);
        let with_miss = |miss: f32| {
            let lake = level(0.0);
            move |x: f32, z: f32| lake(x, z).map(|s| WaterSurface { miss, ..s })
        };
        let deep = float_forces(&f, &at(-3.0), level(0.0), G);
        assert!(deep.force[1] > 0.0);
        assert_eq!(float_forces(&f, &at(-3.0), with_miss(0.2), G), deep);
        assert_eq!(
            float_forces(&f, &at(-3.0), with_miss(0.3), G),
            FloatForces::default()
        );
        assert_eq!(
            float_forces(&f, &at(-3.0), with_miss(f32::NAN), G),
            FloatForces::default()
        );
    }

    /// A flat box settles at the Archimedes draft `ρ_b / ρ · height` within a fifth of a sphere
    /// layer, for light and heavy hulls.
    #[test]
    fn box_settles_at_the_archimedes_draft() {
        let half = [1.0, 0.25, 1.0];
        let f = Floater::box_points(half, [4, 1, 4]);
        assert_eq!(f.points.len(), 16);
        assert!((f.volume() - 8.0 * half[0] * half[1] * half[2]).abs() < 1.0e-4);
        assert!(f.validate().is_ok());
        let f = Floater::box_points([0.5, 0.5, 0.5], [4, 4, 4]);
        for density in [300.0f32, 500.0, 800.0] {
            let mass = density * 1.0;
            let y = equilibrium(&f, mass, Quat::IDENTITY);
            let draft = 0.5 - y;
            let expect = density / FRESH_WATER_DENSITY;
            assert!(
                (draft - expect).abs() < 0.05,
                "density {density}: draft {draft} vs {expect}"
            );
        }
    }

    /// A stable flat hull tilted by 10° about z gets a torque that turns it back.
    #[test]
    fn tilted_hull_rights_itself() {
        let f = Floater::box_points([1.0, 0.25, 1.0], [4, 1, 4]);
        let mass = 0.5 * FRESH_WATER_DENSITY * f.volume();
        let y = equilibrium(&f, mass, Quat::IDENTITY);
        for angle in [10f32.to_radians(), -10f32.to_radians()] {
            let rotation = Quat::from_rotation_z(angle);
            let pose = FloatPose { rotation, ..at(y) };
            let t = float_forces(&f, &pose, level(0.0), G).torque;
            assert!(t[2] * angle < 0.0, "angle {angle}: torque {t:?}");
        }
    }

    /// In a stream a resting floater is dragged downstream; spinning floaters are slowed.
    #[test]
    fn stream_drags_downstream_and_spin_is_damped() {
        let stream = WaterBody::ribbon(
            vec![
                RibbonPoint {
                    position: [-10.0, 0.0, 0.0],
                    width: 6.0,
                    depth: 1.0,
                    speed: Some(1.5),
                },
                RibbonPoint {
                    position: [10.0, 0.0, 0.0],
                    width: 6.0,
                    depth: 1.0,
                    speed: Some(1.5),
                },
            ],
            WaterLook::clear_stream(),
        );
        let water = |x: f32, z: f32| stream.surface(x, z, &WaterClock::default());
        let mut f = Floater::sphere(0.3);
        f.linear_drag = 40.0;
        f.angular_drag = 5.0;
        let pose = FloatPose {
            position: Vec3::ZERO,
            center_of_mass: Vec3::ZERO,
            angular_velocity: Vec3::new(0.0, 2.0, 0.0),
            ..at(0.0)
        };
        let out = float_forces(&f, &pose, water, G);
        assert!(out.force[0] > 0.0 && out.force[2].abs() < 1.0e-3, "{out:?}");
        assert!(out.torque[1] < 0.0, "{out:?}");
        // Pure: the same call gives the same bits.
        assert_eq!(float_forces(&f, &pose, water, G), out);
    }

    #[test]
    fn validation_refuses_bad_floaters() {
        assert!(Floater::sphere(0.5).validate().is_ok());
        assert!(Floater::sphere(0.0).validate().is_err());
        let mut many = Floater::box_points([1.0; 3], [5, 5, 5]);
        assert!(many.validate().is_err(), "125 points");
        many.points.truncate(8);
        assert!(many.validate().is_ok());
        many.fluid_density = f32::NAN;
        assert_eq!(
            many.validate(),
            Err(WaterBodyError::NotFinite("floater.fluid_density"))
        );
        let text = ron::to_string(&Floater::sphere(0.4)).expect("ron");
        let back: Floater = ron::from_str(&text).expect("parse");
        assert_eq!(back, Floater::sphere(0.4));
        let minimal: Floater =
            ron::from_str("(points: [(offset: (0.0, 0.0, 0.0), radius: 1.0)])").expect("parse");
        assert_eq!(minimal.fluid_density, FRESH_WATER_DENSITY);
    }

    /// A bad radius next to a real sphere is skipped, and it does not change the real
    /// sphere's submerged fraction or drag. `volume()` still sums every radius, so a NaN
    /// or a negative cube collapses the divisor `float_forces` uses.
    #[test]
    fn a_bad_radius_beside_a_real_sphere_does_not_change_its_forces() {
        let pose = FloatPose {
            linear_velocity: Vec3::new(1.0, 0.0, 0.0),
            ..at(0.0)
        };
        let good = Floater {
            linear_drag: 10.0,
            ..Floater::sphere(1.0)
        };
        let expected = float_forces(&good, &pose, level(0.0), G);
        assert!(
            (expected.submerged_fraction - 0.5).abs() < 1.0e-5,
            "control fraction {}",
            expected.submerged_fraction
        );
        for bad in [f32::NAN, -1.0] {
            let mixed = Floater {
                points: vec![
                    FloatPoint {
                        offset: [0.0; 3],
                        radius: 1.0,
                    },
                    FloatPoint {
                        offset: [3.0, 0.0, 0.0],
                        radius: bad,
                    },
                ],
                ..good.clone()
            };
            let got = float_forces(&mixed, &pose, level(0.0), G);
            assert_eq!(got.submerged_fraction, expected.submerged_fraction, "{bad}");
            assert_eq!(got.force, expected.force, "{bad}");
            assert_eq!(got.wet_points, expected.wet_points, "{bad}");
        }
    }

    #[test]
    fn non_finite_or_non_positive_radius_is_skipped_not_clamped() {
        let pose = at(0.0);
        for radius in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            let floater = Floater {
                points: vec![FloatPoint {
                    offset: [0.0; 3],
                    radius,
                }],
                ..Floater::box_points([0.5; 3], [1; 3])
            };
            let f = float_forces(&floater, &pose, level(0.0), G);
            assert_eq!(f.force, [0.0; 3], "radius {radius}");
        }
    }

    #[test]
    fn box_points_with_huge_counts_stay_within_the_cap() {
        let f = Floater::box_points([1.0; 3], [10_000; 3]);
        assert!(!f.points.is_empty() && f.points.len() <= 4096);
        assert!(f.validate().is_err());
    }

    /// Spec 87 budgets 64 surface queries. A hand-built list past that still validates as an
    /// error, and the step matches the same floater truncated to the cap.
    #[test]
    fn float_forces_ignores_points_past_the_cap() {
        let many = Floater::box_points([1.0; 3], [5, 5, 5]);
        assert!(many.points.len() > MAX_FLOAT_POINTS);
        let mut capped = many.clone();
        capped.points.truncate(MAX_FLOAT_POINTS);
        let pose = at(-3.0);
        assert_eq!(
            float_forces(&many, &pose, level(0.0), G),
            float_forces(&capped, &pose, level(0.0), G)
        );
        assert!(many.validate().is_err());
        assert!(capped.validate().is_ok());
    }

    /// The queried-volume total skips the same radii the force loop skips. A NaN, infinite,
    /// negative or zero radius beside a real sphere must not change that sphere's submerged
    /// share (a NaN sum collapses through `max` to 1e-12 and clamps the share to 1).
    #[test]
    fn a_skipped_radius_does_not_shrink_the_queried_volume() {
        let good = Floater::sphere(0.5);
        let pose = at(0.0);
        let alone = float_forces(&good, &pose, level(0.0), G);
        assert!(
            (alone.submerged_fraction - 0.5).abs() < 1.0e-3,
            "a center on the waterline is half submerged, share {}",
            alone.submerged_fraction
        );
        for radius in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            let mixed = Floater {
                points: vec![
                    good.points[0],
                    FloatPoint {
                        offset: [2.0, 0.0, 0.0],
                        radius,
                    },
                ],
                ..good.clone()
            };
            let with = float_forces(&mixed, &pose, level(0.0), G);
            assert!(
                (with.submerged_fraction - alone.submerged_fraction).abs() < 1.0e-3,
                "radius {radius} changed the share from {} to {}",
                alone.submerged_fraction,
                with.submerged_fraction
            );
            assert!(
                (with.force[1] - alone.force[1]).abs() < 1.0e-2,
                "radius {radius} changed buoyancy from {} to {}",
                alone.force[1],
                with.force[1]
            );
        }
    }
}
