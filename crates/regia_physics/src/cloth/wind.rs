// SPDX-License-Identifier: MPL-2.0
// Source: J. D. Anderson, "Fundamentals of Aerodynamics" (flat-plate model: drag and lift
// quadratic in the relative speed, normal to and along the plate). Integer hash: a public-domain
// 32-bit avalanche mix (multiply-xorshift). Original code (spec 55 § 2.7).

//! Wind on cloth (spec 55 § 2.7): the per-cloth [`WindModel`] that turns a wind velocity into
//! forces, deterministic [`Gust`]s, and [`WindField`] components that the plugin samples and sums
//! into each solver's wind.
//!
//! Forces are evaluated once per substep from the particle velocities at the substep start. A
//! solver has one wind velocity (its global term plus every field sampled at the solver's
//! particle centroid); per-particle field sampling is a follow-up.

use regia_core::math::{Quat, Vec3, det_powf};
use regia_ecs::prelude::*;

use super::{ClothError, finite, finite_vec};

/// Wind speeds are clamped to this many m/s.
pub const MAX_WIND_SPEED: f32 = 60.0;

/// How a cloth responds to wind (spec 55 § 2.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindModel {
    /// Acceleration `wind * gain` on every free particle, independent of orientation (cheap;
    /// used by far LODs, and the only model that moves ropes).
    Push {
        /// Gain in 1/s (zero or more).
        gain: f32,
    },
    /// Flat-plate drag and lift per triangle, split equally among its three vertices.
    Aerodynamic {
        /// Drag coefficient (zero or more).
        drag: f32,
        /// Lift coefficient (zero or more).
        lift: f32,
    },
}

impl Default for WindModel {
    fn default() -> Self {
        Self::Aerodynamic { drag: 1.0, lift: 0.3 }
    }
}

impl WindModel {
    pub(crate) fn validate(&self) -> Result<(), ClothError> {
        let values: &[f32] = match self {
            Self::Push { gain } => &[*gain],
            Self::Aerodynamic { drag, lift } => &[*drag, *lift],
        };
        for &v in values {
            if finite(v, "wind_model")? < 0.0 {
                return Err(ClothError::BadParam { what: "wind_model" });
            }
        }
        Ok(())
    }

    /// Stable lowercase name (`push`, `aerodynamic`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Push { .. } => "push",
            Self::Aerodynamic { .. } => "aerodynamic",
        }
    }
}

/// Deterministic gust modulation of a wind speed: a pure function of `(seed, fixed step,
/// quantized position)` built from an integer hash and a smoothstep blend between keys
/// `period_steps` apart. No trigonometry and no clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gust {
    /// Relative amplitude: the speed is scaled by a factor in `1 ± amplitude` (0..1).
    pub amplitude: f32,
    /// Fixed steps between gust keys (at least 1).
    pub period_steps: u32,
    /// Seed of the hash.
    pub seed: u32,
}

impl Gust {
    /// Validates the gust.
    ///
    /// # Errors
    /// [`ClothError::BadParam`] for an amplitude outside `0..=1` or a zero period;
    /// [`ClothError::NonFinite`] for NaN.
    pub fn validate(&self) -> Result<(), ClothError> {
        let a = finite(self.amplitude, "gust")?;
        if !(0.0..=1.0).contains(&a) || self.period_steps == 0 {
            return Err(ClothError::BadParam { what: "gust" });
        }
        Ok(())
    }

    /// The speed factor at fixed step `step` and world position `at` (positions are quantized
    /// to 1 m cells, so a cloth sees one coherent gust).
    #[must_use]
    pub fn factor(&self, step: u64, at: Vec3) -> f32 {
        let period = u64::from(self.period_steps.max(1));
        let key = step / period;
        let frac = (step % period) as f32 / period as f32;
        let cell = at.floor();
        // `as` saturates for huge coordinates, which keeps the hash input defined.
        let cell = [cell.x as i32, cell.y as i32, cell.z as i32];
        let v0 = hash01(self.seed, key, cell);
        let v1 = hash01(self.seed, key.wrapping_add(1), cell);
        let s = frac * frac * (3.0 - 2.0 * frac);
        let v = v0 + (v1 - v0) * s;
        1.0 + self.amplitude * (2.0 * v - 1.0)
    }
}

fn mix(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// A hash of `(seed, key, cell)` mapped to `[0, 1]`.
fn hash01(seed: u32, key: u64, cell: [i32; 3]) -> f32 {
    let mut h = mix(seed ^ 0x9e37_79b9);
    h = mix(h ^ key as u32);
    h = mix(h ^ (key >> 32) as u32);
    for c in cell {
        h = mix(h ^ c as u32);
    }
    (h >> 8) as f32 / ((1u32 << 24) - 1) as f32
}

/// Shape of a [`WindField`] (spec 55 § 2.7).
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub enum WindFieldKind {
    /// Wind along the entity's forward axis (-Z) at `speed` m/s.
    Directional {
        /// Speed in m/s.
        speed: f32,
    },
    /// Wind away from the entity (toward it when `strength` is negative), magnitude
    /// `strength * (1 - d / radius)^falloff` inside `radius`.
    Radial {
        /// Peak speed in m/s at the center.
        strength: f32,
        /// Falloff exponent (zero or more).
        falloff: f32,
    },
    /// Wind circling the entity's local `axis` (right-handed), magnitude as for `Radial`.
    Vortex {
        /// Peak speed in m/s.
        strength: f32,
        /// Local rotation axis (normalized when sampled).
        axis: Vec3,
        /// Falloff exponent (zero or more).
        falloff: f32,
    },
}

/// A wind source placed in the world. The cloth plugin samples every field at each solver's
/// particle centroid, sums the samples of overlapping fields in a deterministic order and adds
/// the [`ClothSet`] global wind (`ClothSet::set_global_wind`). The fold order is never
/// entity-id or query order: fields with an `EntityGuid` first in full 128-bit guid order,
/// then fields without one in `SpawnOrder` arrival order (see the `gather_cloth_wind` docs).
///
/// [`ClothSet`]: super::ClothSet
#[derive(Component, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Component)]
pub struct WindField {
    /// Shape and strength.
    pub kind: WindFieldKind,
    /// Radius of the field's sphere of influence in meters; `None` = everywhere (radial and
    /// vortex fields then use no falloff).
    pub radius: Option<f32>,
    /// Optional gust modulation.
    #[reflect(ignore)]
    pub gust: Option<Gust>,
}

impl Default for WindField {
    fn default() -> Self {
        Self {
            kind: WindFieldKind::Directional { speed: 5.0 },
            radius: None,
            gust: None,
        }
    }
}

impl WindField {
    /// The wind velocity this field gives at `point`, for a field at `origin` with `rotation`,
    /// at fixed step `step`. Non-finite results are zero; the speed is clamped to
    /// [`MAX_WIND_SPEED`].
    #[must_use]
    pub fn sample(&self, origin: Vec3, rotation: Quat, point: Vec3, step: u64) -> Vec3 {
        let offset = point - origin;
        let d = offset.length();
        let weight = |falloff: f32| match self.radius {
            Some(r) if r > 0.0 => {
                if d >= r {
                    0.0
                } else {
                    // Deterministic powers only: `powf` (and even `powf(x, 2.0)`) may differ
                    // in the last ulp between platform libms, and the weight is integrated
                    // into the sim. Small integer falloffs multiply; anything else takes the
                    // fixed software kernel (ADR-4773).
                    let base = 1.0 - d / r;
                    let falloff = falloff.max(0.0);
                    if falloff.fract() == 0.0 && falloff <= 8.0 {
                        base.powi(falloff as i32)
                    } else {
                        det_powf(base, falloff)
                    }
                }
            }
            Some(_) => 0.0,
            None => 1.0,
        };
        let w = match self.kind {
            WindFieldKind::Directional { speed } => {
                let inside = self.radius.is_none_or(|r| d < r);
                if inside {
                    rotation * Vec3::NEG_Z * speed
                } else {
                    Vec3::ZERO
                }
            }
            WindFieldKind::Radial { strength, falloff } => {
                offset.normalize_or_zero() * (strength * weight(falloff))
            }
            WindFieldKind::Vortex {
                strength,
                axis,
                falloff,
            } => {
                let axis = (rotation * axis).normalize_or_zero();
                axis.cross(offset).normalize_or_zero() * (strength * weight(falloff))
            }
        };
        let w = match self.gust {
            Some(g) => w * g.factor(step, point),
            None => w,
        };
        if finite_vec(w, "wind").is_err() {
            return Vec3::ZERO;
        }
        w.clamp_length_max(MAX_WIND_SPEED)
    }
}

/// Wind force on one triangle (spec 55 § 2.7), returned per vertex (equal thirds).
///
/// `v_r = wind - v_t`, `v_n = v_r · n`; drag `0.5 ρ A drag |v_n| v_n n`, lift
/// `0.5 ρ A lift |v_n| t` with `t` the unit tangential part of `v_r`. Both faces respond.
#[must_use]
pub(crate) fn triangle_force(p: [Vec3; 3], v: [Vec3; 3], wind: Vec3, rho: f32, drag: f32, lift: f32) -> Vec3 {
    let cross = (p[1] - p[0]).cross(p[2] - p[0]);
    let len = cross.length();
    if len <= 1e-12 {
        return Vec3::ZERO;
    }
    let n = cross / len;
    let area = 0.5 * len;
    let v_t = (v[0] + v[1] + v[2]) / 3.0;
    let v_r = wind - v_t;
    let v_n = v_r.dot(n);
    let q = 0.5 * rho * area * v_n.abs();
    let tangent = (v_r - n * v_n).normalize_or_zero();
    (n * (drag * v_n) + tangent * lift) * q / 3.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gust_is_a_pure_function_of_seed_step_and_cell() {
        let g = Gust {
            amplitude: 0.5,
            period_steps: 30,
            seed: 7,
        };
        for step in [0u64, 1, 29, 30, 31, 1000] {
            let a = g.factor(step, Vec3::new(0.2, 1.4, -3.3));
            let b = g.factor(step, Vec3::new(0.7, 1.9, -3.1));
            assert_eq!(a.to_bits(), b.to_bits(), "same cell, same step");
            assert!((0.5..=1.5).contains(&a), "{a}");
        }
        // Keys are hit exactly at period boundaries; values in between are blended.
        let other = Gust { seed: 8, ..g };
        let differs = (0..10u64).any(|k| g.factor(k * 30, Vec3::ZERO) != other.factor(k * 30, Vec3::ZERO));
        assert!(differs, "seed matters");
    }

    #[test]
    fn plate_drag_matches_the_flat_plate_formula() {
        let p = [Vec3::ZERO, Vec3::X, Vec3::Y];
        let v = [Vec3::ZERO; 3];
        let f = triangle_force(p, v, Vec3::Z * 10.0, 1.2, 1.0, 0.0) * 3.0;
        // A = 0.5, n = +Z: 0.5 * 1.2 * 0.5 * 100 = 30 N along +Z.
        assert!((f - Vec3::Z * 30.0).length() < 1e-4, "{f}");
        // Wind in the plate's plane: no drag.
        let f = triangle_force(p, v, Vec3::X * 10.0, 1.2, 1.0, 0.3);
        assert!(f.length() < 1e-6, "{f}");
        // The back face responds the same way.
        let f = triangle_force(p, v, Vec3::NEG_Z * 10.0, 1.2, 1.0, 0.0) * 3.0;
        assert!((f + Vec3::Z * 30.0).length() < 1e-4, "{f}");
    }

    #[test]
    fn fields_point_the_documented_way() {
        let d = WindField::default();
        assert_eq!(
            d.sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0),
            Vec3::NEG_Z * 5.0
        );
        let radial = WindField {
            kind: WindFieldKind::Radial {
                strength: 4.0,
                falloff: 1.0,
            },
            radius: Some(2.0),
            gust: None,
        };
        let w = radial.sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0);
        assert!((w - Vec3::X * 2.0).length() < 1e-6, "{w}");
        assert_eq!(
            radial.sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X * 3.0, 0),
            Vec3::ZERO
        );
        let vortex = WindField {
            kind: WindFieldKind::Vortex {
                strength: 3.0,
                axis: Vec3::Y,
                falloff: 0.0,
            },
            radius: None,
            gust: None,
        };
        let w = vortex.sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0);
        assert!((w - Vec3::NEG_Z * 3.0).length() < 1e-6, "{w}");
        let strong = WindField {
            kind: WindFieldKind::Directional { speed: 500.0 },
            radius: None,
            gust: None,
        };
        let w = strong.sample(Vec3::ZERO, Quat::IDENTITY, Vec3::ZERO, 0);
        assert!((w.length() - MAX_WIND_SPEED).abs() < 1e-3);
    }

    /// `10 * (3/4)^2.5` along +X via the deterministic kernel, pinned (ADR-4773, audit G
    /// finding 4): `powf` may differ in the last ulp between platform libms.
    const RADIAL_2_5_BITS: u32 = 1_083_957_875;

    #[test]
    fn radial_and_vortex_falloff_use_deterministic_powers() {
        let radial = |falloff: f32| WindField {
            kind: WindFieldKind::Radial {
                strength: 10.0,
                falloff,
            },
            radius: Some(4.0),
            gust: None,
        };
        let w = radial(2.5).sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0);
        assert_eq!(w.y.to_bits(), 0);
        assert_eq!(w.z.to_bits(), 0);
        assert_eq!(
            w.x.to_bits(),
            RADIAL_2_5_BITS,
            "(1 - 1/4)^2.5 must come from the software kernel, never powf"
        );
        // Small integer falloffs multiply exactly (the powi path).
        let w = radial(2.0).sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0);
        assert_eq!(w, Vec3::X * 5.625, "0.75² = 0.5625 exactly");
        let w = radial(0.0).sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0);
        assert_eq!(w, Vec3::X * 10.0, "a zero falloff is an unweighted field");
        // The vortex shares the weight path: same weight, circling (−Z at +X around +Y).
        let vortex = WindField {
            kind: WindFieldKind::Vortex {
                strength: 10.0,
                axis: Vec3::Y,
                falloff: 2.5,
            },
            radius: Some(4.0),
            gust: None,
        };
        let w = vortex.sample(Vec3::ZERO, Quat::IDENTITY, Vec3::X, 0);
        assert_eq!(w.x.to_bits(), 0);
        assert_eq!(w.y.to_bits(), 0);
        assert_eq!(
            w.z.to_bits(),
            RADIAL_2_5_BITS ^ 0x8000_0000,
            "same magnitude as the radial field, negated by the cross product"
        );
    }
}
