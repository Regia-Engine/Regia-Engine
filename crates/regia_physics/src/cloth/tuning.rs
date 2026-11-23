// SPDX-License-Identifier: MPL-2.0
//! [`ClothTuning`]: the solver settings that are not in the baseline [`ClothConfig`]
//! (`docs/specs/55-cloth-and-soft-body.md` § 2.3). Held by the solver, set with
//! [`ClothSolver::set_tuning`], validated as a whole (a rejected tuning changes nothing).
//!
//! [`ClothConfig`]: super::ClothConfig
//! [`ClothSolver::set_tuning`]: super::ClothSolver::set_tuning

use regia_core::math::Vec3;

use super::wind::{MAX_WIND_SPEED, WindModel};
use super::{ClothError, finite, finite_vec, non_negative};

/// Self-collision mode of a solver (spec 55 § 2.6).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum SelfCollision {
    /// No self-collision (default).
    #[default]
    Off,
    /// Particle pairs closer than `2 * radius` are pushed apart along their separation.
    Spheres {
        /// Particle radius in meters (positive).
        radius: f32,
    },
    /// Particles are kept `radius` away from nearby sim triangles, on the side of the plane they
    /// were on at the start of the fixed step (folds and layered garments, ADR-2425). Needs the
    /// solver's triangles (grids and cooked assets add them).
    VertexTriangle {
        /// Contact distance in meters (positive).
        radius: f32,
    },
}

impl SelfCollision {
    /// The radius, or `None` for [`SelfCollision::Off`].
    #[must_use]
    pub fn radius(&self) -> Option<f32> {
        match *self {
            Self::Off => None,
            Self::Spheres { radius } | Self::VertexTriangle { radius } => Some(radius),
        }
    }

    /// One level cheaper (`VertexTriangle` -> `Spheres` -> `Off`), used by distance LOD 1.
    #[must_use]
    pub fn step_down(&self) -> Self {
        match *self {
            Self::VertexTriangle { radius } => Self::Spheres { radius },
            Self::Spheres { .. } | Self::Off => Self::Off,
        }
    }

    /// Stable lowercase name (`off`, `spheres`, `vertex_triangle`) for the agent protocol.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Spheres { .. } => "spheres",
            Self::VertexTriangle { .. } => "vertex_triangle",
        }
    }
}

/// Optional hard cap on edge length ratios, projected after the last iteration of each
/// substep (spec 55 § 2.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrainCap {
    /// Largest allowed `length / rest length` (at least 1).
    pub max: f32,
    /// Smallest allowed `length / rest length` (in `0..=1`).
    pub min: f32,
}

/// Optional edge tearing (spec 55 § 2.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TearParams {
    /// A distance link whose length ratio exceeds this at the end of a fixed step is cut
    /// (greater than 1; the spec's default is 2.0).
    pub strain: f32,
    /// Most links cut per fixed step, in ascending constraint index (at least 1).
    pub max_per_step: u32,
}

impl Default for TearParams {
    fn default() -> Self {
        Self {
            strain: 2.0,
            max_per_step: 4,
        }
    }
}

/// Settings beyond the baseline [`ClothConfig`](super::ClothConfig): pose binding, strain caps,
/// tethers, self-collision, wind response, teleport and tearing. Defaults follow spec 55 § 2.3.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothTuning {
    /// Scales the config gravity (default 1, range -4..4).
    pub gravity_scale: f32,
    /// Replaces the config gravity when set (scaled by `gravity_scale` too).
    pub gravity_override: Option<Vec3>,
    /// Compliance of the soft pose attachment (`pose_pull` map), default 1e-3.
    pub pull_compliance: f32,
    /// Runtime multiplier of the `pose_pull` map (default 1; 0 switches the pull off).
    pub pull_scale: f32,
    /// How much world-space inertia free particles keep when the root moves (default 1 = all;
    /// 0 = the cloth moves rigidly with the root). Range 0..1.
    pub inertia_scale: f32,
    /// Optional hard cap on edge length ratios.
    pub strain_cap: Option<StrainCap>,
    /// Optional tether scale (at least 1): each free particle stays within `scale` times its
    /// rest path length from the nearest pinned particle.
    pub tether: Option<f32>,
    /// Self-collision mode.
    pub self_collision: SelfCollision,
    /// Self-collision friction (default 0.2, range 0..2).
    pub self_friction: f32,
    /// Pairs within this many edge hops are never tested for self-collision (default 2).
    pub exclude_rings: u32,
    /// How triangles respond to wind (default `Aerodynamic { drag: 1.0, lift: 0.3 }`).
    pub wind_model: WindModel,
    /// Air density in kg/m³ for aerodynamic forces (default 1.2, positive).
    pub air_density: f32,
    /// A root (or pose target) jump longer than this in one fixed step snaps the cloth to its
    /// pose (default 2.0 m, positive).
    pub teleport_distance: f32,
    /// Optional tearing.
    pub tear: Option<TearParams>,
}

impl Default for ClothTuning {
    fn default() -> Self {
        Self {
            gravity_scale: 1.0,
            gravity_override: None,
            pull_compliance: 1e-3,
            pull_scale: 1.0,
            inertia_scale: 1.0,
            strain_cap: None,
            tether: None,
            self_collision: SelfCollision::Off,
            self_friction: 0.2,
            exclude_rings: 2,
            wind_model: WindModel::default(),
            air_density: 1.2,
            teleport_distance: 2.0,
            tear: None,
        }
    }
}

fn bad(what: &'static str) -> ClothError {
    ClothError::BadParam { what }
}

fn in_range(value: f32, lo: f32, hi: f32, what: &'static str) -> Result<f32, ClothError> {
    let value = finite(value, what)?;
    if (lo..=hi).contains(&value) {
        Ok(value)
    } else {
        Err(bad(what))
    }
}

impl ClothTuning {
    /// Checks every field against the ranges of spec 55 § 2.3.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] for NaN or infinity, [`ClothError::Negative`] for a negative
    /// compliance, [`ClothError::BadParam`] for any other value out of range.
    pub fn validate(&self) -> Result<(), ClothError> {
        in_range(self.gravity_scale, -4.0, 4.0, "gravity_scale")?;
        if let Some(g) = self.gravity_override {
            finite_vec(g, "gravity_override")?;
        }
        non_negative(self.pull_compliance, "pull_compliance")?;
        non_negative(self.pull_scale, "pull_scale")?;
        in_range(self.inertia_scale, 0.0, 1.0, "inertia_scale")?;
        if let Some(cap) = self.strain_cap {
            if finite(cap.max, "strain_cap")? < 1.0 {
                return Err(bad("strain_cap"));
            }
            in_range(cap.min, 0.0, 1.0, "strain_cap")?;
        }
        if let Some(scale) = self.tether
            && finite(scale, "tether")? < 1.0
        {
            return Err(bad("tether"));
        }
        if let Some(radius) = self.self_collision.radius()
            && finite(radius, "self_collision")? <= 0.0
        {
            return Err(bad("self_collision"));
        }
        in_range(self.self_friction, 0.0, 2.0, "self_friction")?;
        if self.exclude_rings > 16 {
            return Err(bad("exclude_rings"));
        }
        self.wind_model.validate()?;
        if finite(self.air_density, "air_density")? <= 0.0 {
            return Err(bad("air_density"));
        }
        if finite(self.teleport_distance, "teleport_distance")? <= 0.0 {
            return Err(bad("teleport_distance"));
        }
        if let Some(tear) = self.tear
            && (finite(tear.strain, "tear")? <= 1.0 || tear.max_per_step == 0)
        {
            return Err(bad("tear"));
        }
        Ok(())
    }

    /// The gravity a solver with config gravity `config` applies.
    #[must_use]
    pub fn gravity(&self, config: Vec3) -> Vec3 {
        self.gravity_override.unwrap_or(config) * self.gravity_scale
    }

    /// Feeds a canonical encoding of every field into `write` (state digest, spec 55 § 7).
    pub(crate) fn encode(&self, write: &mut impl FnMut(&[u8])) {
        let mut f = |v: f32| write(&v.to_bits().to_le_bytes());
        f(self.gravity_scale);
        match self.gravity_override {
            Some(g) => {
                f(1.0);
                g.to_array().into_iter().for_each(&mut f);
            }
            None => f(0.0),
        }
        f(self.pull_compliance);
        f(self.pull_scale);
        f(self.inertia_scale);
        match self.strain_cap {
            Some(cap) => [1.0, cap.max, cap.min].into_iter().for_each(&mut f),
            None => f(0.0),
        }
        f(self.tether.unwrap_or(0.0));
        match self.self_collision {
            SelfCollision::Off => f(0.0),
            SelfCollision::Spheres { radius } => [1.0, radius].into_iter().for_each(&mut f),
            SelfCollision::VertexTriangle { radius } => [2.0, radius].into_iter().for_each(&mut f),
        }
        f(self.self_friction);
        f(self.exclude_rings as f32);
        match self.wind_model {
            WindModel::Push { gain } => [1.0, gain].into_iter().for_each(&mut f),
            WindModel::Aerodynamic { drag, lift } => [2.0, drag, lift].into_iter().for_each(&mut f),
        }
        f(self.air_density);
        f(self.teleport_distance);
        match self.tear {
            Some(t) => [1.0, t.strain, t.max_per_step as f32]
                .into_iter()
                .for_each(&mut f),
            None => f(0.0),
        }
    }
}

/// Clamps a wind vector to [`MAX_WIND_SPEED`].
pub(crate) fn clamp_wind(w: Vec3) -> Vec3 {
    w.clamp_length_max(MAX_WIND_SPEED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_ranges_are_enforced() {
        assert_eq!(ClothTuning::default().validate(), Ok(()));
        let bad_cases = [
            ClothTuning {
                gravity_scale: 5.0,
                ..ClothTuning::default()
            },
            ClothTuning {
                inertia_scale: 1.5,
                ..ClothTuning::default()
            },
            ClothTuning {
                strain_cap: Some(StrainCap { max: 0.9, min: 0.5 }),
                ..ClothTuning::default()
            },
            ClothTuning {
                tether: Some(0.5),
                ..ClothTuning::default()
            },
            ClothTuning {
                self_collision: SelfCollision::Spheres { radius: 0.0 },
                ..ClothTuning::default()
            },
            ClothTuning {
                air_density: 0.0,
                ..ClothTuning::default()
            },
            ClothTuning {
                tear: Some(TearParams {
                    strain: 1.0,
                    max_per_step: 1,
                }),
                ..ClothTuning::default()
            },
        ];
        for tuning in bad_cases {
            let err = tuning.validate().expect_err("out of range");
            assert_eq!(err.code(), "cloth_bad_param", "{tuning:?}");
        }
        let nan = ClothTuning {
            pull_compliance: f32::NAN,
            ..ClothTuning::default()
        };
        assert_eq!(nan.validate().map_err(|e| e.code()), Err("cloth_non_finite"));
    }

    #[test]
    fn self_collision_steps_down_one_level() {
        let vt = SelfCollision::VertexTriangle { radius: 0.01 };
        assert_eq!(vt.step_down(), SelfCollision::Spheres { radius: 0.01 });
        assert_eq!(vt.step_down().step_down(), SelfCollision::Off);
        assert_eq!(SelfCollision::Off.name(), "off");
    }
}
