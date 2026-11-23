// SPDX-License-Identifier: MPL-2.0
// Source: M. Macklin, M. Müller, N. Chentanez, "XPBD", MIG 2016 (the soft attachment is a
// compliant zero-rest-length distance constraint to a moving target); M. Müller et al.,
// "Position Based Dynamics", 2007 (hard inequality constraints projected to the feasible set:
// the reach sphere and the standoff half-space). Original code (spec 55 § 2.2).

//! Cloth bound to a skinned pose (spec 55 § 2.2): per-vertex [`PoseTarget`]s supplied by the
//! game (or the framework glue that skins the sim mesh), the [`PoseMaps`] that bind particles to
//! them, the optional root motion used for root inertia and teleport detection, and the pose
//! constraints projected inside each solver iteration.

use regia_core::math::{Isometry, Vec3};

use super::{ClothError, ClothParticle, EPSILON, finite, finite_vec};

/// Where a particle would be if it were only skinned: a world-space position and normal.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PoseTarget {
    /// World-space position in meters.
    pub position: Vec3,
    /// World-space unit normal (a zero normal disables the standoff half-space there).
    pub normal: Vec3,
}

/// Per-particle maps that bind a cloth to its pose (spec 55 § 2.2). Each map is either empty
/// (not used: the default below applies to every particle) or holds one value per particle.
///
/// | Map | Default when empty | Range |
/// |---|---|---|
/// | `reach` | unlimited | meters, >= 0; 0 makes the particle follow its target exactly |
/// | `standoff` | none | meters, signed |
/// | `pose_pull` | 0 (no pull) | `[0, 1]` |
/// | `sim_mix` | 1 | `[0, 1]` |
/// | `mass_scale` | 1 | `> 0` |
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoseMaps {
    /// Radius in meters of the sphere around the target a particle must stay inside.
    pub reach: Vec<f32>,
    /// The particle must satisfy `dot(x - target, n) >= -standoff`.
    pub standoff: Vec<f32>,
    /// Strength of the soft attachment to the target.
    pub pose_pull: Vec<f32>,
    /// Mix of simulated and pose position in the output (0 = pose).
    pub sim_mix: Vec<f32>,
    /// Multiplies particle mass.
    pub mass_scale: Vec<f32>,
}

impl PoseMaps {
    /// Checks lengths (each map empty or `particles` long) and ranges.
    ///
    /// # Errors
    /// [`ClothError::MapSizeMismatch`] for a wrong length, [`ClothError::NonFinite`] for NaN or
    /// infinity, [`ClothError::BadParam`] for a value out of range.
    pub fn validate(&self, particles: usize) -> Result<(), ClothError> {
        type Check = fn(f32) -> bool;
        let maps: [(&'static str, &[f32], Check); 5] = [
            ("reach", &self.reach, |v| v >= 0.0),
            ("standoff", &self.standoff, |_| true),
            ("pose_pull", &self.pose_pull, |v| (0.0..=1.0).contains(&v)),
            ("sim_mix", &self.sim_mix, |v| (0.0..=1.0).contains(&v)),
            ("mass_scale", &self.mass_scale, |v| v > 0.0),
        ];
        for (what, values, ok) in maps {
            if !values.is_empty() && values.len() != particles {
                return Err(ClothError::MapSizeMismatch {
                    map: what,
                    len: values.len(),
                    expected: particles,
                });
            }
            for &v in values {
                if !ok(finite(v, what)?) {
                    return Err(ClothError::BadParam { what });
                }
            }
        }
        Ok(())
    }

    fn get(map: &[f32], i: usize, default: f32) -> f32 {
        map.get(i).copied().unwrap_or(default)
    }

    /// Reach of particle `i` (`f32::INFINITY` when the map is empty).
    #[must_use]
    pub fn reach_of(&self, i: usize) -> f32 {
        Self::get(&self.reach, i, f32::INFINITY)
    }

    /// `sim_mix` of particle `i` (1 when the map is empty).
    #[must_use]
    pub fn sim_mix_of(&self, i: usize) -> f32 {
        Self::get(&self.sim_mix, i, 1.0)
    }

    /// `mass_scale` of particle `i` (1 when the map is empty).
    #[must_use]
    pub fn mass_scale_of(&self, i: usize) -> f32 {
        Self::get(&self.mass_scale, i, 1.0)
    }
}

/// The pose binding a solver holds: maps, the targets of the previous and current fixed step,
/// the targets interpolated for the running substep, and the base inverse masses the maps
/// scale.
#[derive(Debug, Clone, Default)]
pub(crate) struct PoseBinding {
    pub maps: Option<PoseMaps>,
    /// Inverse masses before `reach == 0` and `mass_scale` were applied.
    pub base_inv_mass: Vec<f32>,
    pub prev: Vec<PoseTarget>,
    pub cur: Vec<PoseTarget>,
    /// Targets at the end of the running substep.
    pub sub: Vec<PoseTarget>,
    /// Soft-attachment Lagrange multipliers (reset per substep).
    pub pull_lambda: Vec<f32>,
    pub root_prev: Option<Isometry>,
    pub root_cur: Option<Isometry>,
}

impl PoseBinding {
    pub fn has_targets(&self) -> bool {
        !self.cur.is_empty()
    }

    /// Effective inverse mass of particle `i` under the maps.
    pub fn effective_inv_mass(&self, i: usize, base: f32) -> f32 {
        match &self.maps {
            Some(maps) if maps.reach_of(i) == 0.0 => 0.0,
            Some(maps) => base / maps.mass_scale_of(i),
            None => base,
        }
    }

    /// Re-expresses every world-space target and the root motion after an origin rebase.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for target in self.prev.iter_mut().chain(&mut self.cur).chain(&mut self.sub) {
            target.position -= shift;
        }
        for root in [&mut self.root_prev, &mut self.root_cur].into_iter().flatten() {
            root.translation -= shift;
        }
    }

    /// Fills `sub` with the targets at substep fraction `t` (linear in position, normalized
    /// linear in normal).
    pub fn interpolate(&mut self, t: f32) {
        self.sub.clear();
        for (a, b) in self.prev.iter().zip(&self.cur) {
            let position = if t >= 1.0 {
                b.position
            } else {
                a.position.lerp_monotonic(b.position, t)
            };
            let normal = if t >= 1.0 {
                b.normal
            } else {
                a.normal.lerp(b.normal, t).normalize_or_zero()
            };
            self.sub.push(PoseTarget { position, normal });
        }
    }

    /// Root pose at fraction `t` of the fixed step (normalized linear interpolation of the
    /// rotation: no trigonometry in stepping).
    pub fn root_at(&self, t: f32) -> Option<Isometry> {
        let (a, b) = (self.root_prev?, self.root_cur?);
        let mut rb = b.rotation;
        if a.rotation.dot(rb) < 0.0 {
            rb = -rb;
        }
        Some(Isometry::new(
            a.rotation.lerp(rb, t).normalize(),
            a.translation.lerp(b.translation, t),
        ))
    }
}

/// Checks a target list: one entry per particle, every position and normal finite.
///
/// Does not allocate and does not write. Callers validate both lists before filling, so a later
/// error leaves the stored targets untouched.
pub(crate) fn validate_targets(targets: &[PoseTarget], particles: usize) -> Result<(), ClothError> {
    if targets.len() != particles {
        return Err(ClothError::PoseSizeMismatch {
            len: targets.len(),
            expected: particles,
        });
    }
    for target in targets {
        finite_vec(target.position, "pose target")?;
        finite_vec(target.normal, "pose normal")?;
    }
    Ok(())
}

/// Writes `targets` into `dst`, normalizing normals (zero stays zero) and keeping `dst`'s allocation.
pub(crate) fn write_targets(dst: &mut Vec<PoseTarget>, targets: &[PoseTarget]) {
    dst.clear();
    dst.extend(targets.iter().map(|target| PoseTarget {
        position: target.position,
        normal: target.normal.normalize_or_zero(),
    }));
}

/// Projects the pose constraints for every free particle, in index order: the soft pull, then
/// the hard reach sphere, then the hard standoff half-space (spec 55 § 2.2 step 3).
pub(crate) fn solve_pose(
    particles: &mut [ClothParticle],
    binding: &mut PoseBinding,
    pull_compliance: f32,
    pull_scale: f32,
    inv_h2: f32,
) {
    let Some(maps) = binding.maps.as_ref() else {
        return;
    };
    if binding.sub.len() != particles.len() {
        return;
    }
    for (i, p) in particles.iter_mut().enumerate() {
        if p.inv_mass == 0.0 {
            continue;
        }
        let target = binding.sub[i];
        // Soft attachment: C = |x - t| with compliance pull_compliance / max(pose_pull, 1e-6).
        let pull = PoseMaps::get(&maps.pose_pull, i, 0.0) * pull_scale;
        if pull > 0.0 {
            let d = p.position - target.position;
            let len = d.length();
            // An overflowing compliance pulls with zero force (see `xpbd_alpha`).
            if len > EPSILON
                && let Some(alpha) = super::xpbd_alpha(pull_compliance / pull.max(1e-6), inv_h2)
            {
                let lambda = &mut binding.pull_lambda[i];
                let dlambda = (-len - alpha * *lambda) / (p.inv_mass + alpha);
                *lambda += dlambda;
                p.position += d / len * (dlambda * p.inv_mass);
            }
        }
        // Reach sphere.
        let reach = maps.reach_of(i);
        if reach.is_finite() {
            let d = p.position - target.position;
            let len = d.length();
            if len > reach && len > EPSILON {
                p.position = target.position + d * (reach / len);
            }
        }
        // Standoff half-space.
        if let Some(&standoff) = maps.standoff.get(i)
            && target.normal != Vec3::ZERO
        {
            let depth = (p.position - target.position).dot(target.normal);
            if depth < -standoff {
                p.position += target.normal * (-standoff - depth);
            }
        }
    }
}
