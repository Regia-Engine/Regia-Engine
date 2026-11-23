// SPDX-License-Identifier: MPL-2.0
// Source: M. Macklin, M. Müller, N. Chentanez, "XPBD: Position-Based Simulation of Compliant
// Constrained Dynamics", Motion in Games 2016 (compliant constraint update with a per-constraint
// Lagrange multiplier, eq. 18, Algorithm 1); M. Müller, B. Heidelberger, M. Hennix, J. Ratcliff,
// "Position Based Dynamics", VRIPHYS 2006 / JVCIR 2007 (the predict-project-update loop, the
// dihedral bending constraint of § 4.2, collision projection and velocity-level friction of
// § 3.5). Gradients re-derived from the constraint definitions; FNV-1a digest (public domain).

//! Deterministic CPU cloth and rope solver (extended position-based dynamics, XPBD).
//!
//! A [`ClothSolver`] owns particles (position, velocity, inverse mass; inverse mass 0 pins a
//! particle), distance constraints (stretch and shear), dihedral bending constraints and a
//! list of collision shapes (spheres, capsules, infinite planes) with Coulomb friction.
//! [`ClothSolver::add_grid`] builds a rectangular cloth and [`ClothSolver::add_rope`] a rope.
//!
//! # Time stepping
//!
//! [`ClothSolver::step`] adds the frame time to an accumulator and runs whole fixed steps of
//! [`ClothConfig::time_step`] (at most [`ClothConfig::max_steps_per_call`]; the excess is
//! dropped and counted in [`ClothStats::dropped_steps`]). Each fixed step is split into
//! [`ClothConfig::substeps`] substeps of length `h`, and each substep:
//!
//! 1. integrates gravity and damping into the velocity and predicts positions;
//! 2. resets every constraint's Lagrange multiplier;
//! 3. runs [`ClothConfig::iterations`] Gauss-Seidel passes over the distance constraints, the
//!    bending constraints and then the collision shapes (collision last, so a step never ends
//!    inside a shape);
//! 4. derives velocities from the position change; particles that touched a shape get an
//!    inelastic contact (the push out of a shape never becomes launch speed) and friction.
//!
//! # Compliance
//!
//! Every constraint has a compliance `α` (inverse stiffness, in meters per newton for distance
//! constraints and radians per newton-meter for bending). `0` is perfectly stiff; larger values
//! are softer. Because the update uses `α / h²`, stiffness does not change with the substep
//! count or the iteration count, which is the point of XPBD over plain position-based dynamics.
//!
//! # Determinism
//!
//! Every loop runs in insertion order over plain vectors; no hash maps, threads or clocks are
//! involved. The same build on the same platform with the same calls gives bit-identical
//! particles, which [`ClothSolver::state_digest`] summarizes in one value.
//!
//! # Skinned cloth, tuning and the rest of spec 55
//!
//! Beyond the baseline loop a solver can hold (`docs/specs/55-cloth-and-soft-body.md`):
//!
//! - a pose binding ([`ClothSolver::set_pose_targets`], [`ClothSolver::set_pose_maps`]):
//!   per-particle [`PoseTarget`]s interpolated per substep, the `reach` sphere, `standoff`
//!   half-space and soft `pose_pull` attachment, a blend between pose and simulation
//!   ([`ClothSolver::set_blend`], [`ClothSolver::output`]), root inertia and teleport resets;
//! - a [`ClothTuning`]: gravity scale, strain caps, tethers, self-collision, the wind model,
//!   teleport distance and tearing;
//! - a wind velocity ([`ClothSolver::set_wind`], [`ClothSet::set_global_wind`],
//!   [`WindField`]s);
//! - a lifecycle [`ClothState`] with blend ramps, pause and distance LOD
//!   ([`ClothSolver::set_lod_level`]).
//!
//! Each substep then runs: pose interpolation and root inertia, prediction (gravity, wind,
//! damping), and per iteration distance, bend, tethers, pose constraints, self-collision and
//! colliders; strain caps after the last iteration; velocities and contacts. Tearing is checked
//! at the end of each fixed step.
//!
//! # Limits
//!
//! - No cloth-versus-cloth collision (two solvers never touch); within one solver
//!   `SelfCollision::VertexTriangle` keeps particles on their side of nearby sim triangles and
//!   per-particle garment layers ([`ClothSolver::set_layers`]) make contacts one-sided
//!   (ADR-2425). Edge-edge crossings are not tested.
//! - A collider (moving [`ClothCollider`] or snapshot shape) whose travel within one substep at a
//!   particle exceeds the thickness is tested along the particle's relative path (swept), so a
//!   fast collider carries the cloth instead of passing through it.
//! - Moving colliders: with [`ClothSolver::set_collider_previous`] a collider is interpolated
//!   from its previous to its current shape across the substeps and friction uses its velocity;
//!   without it the collider is static within the step (the baseline).
//! - The bending constraint uses the unsigned dihedral angle: a hinge whose rest angle is not
//!   flat restores toward the mirrored angle too when folded through flat.
//!
//! [`ClothPlugin`] steps every solver of a [`ClothSet`] resource in `FixedUpdate`; the agent
//! protocol reads the same resource (`physics.cloth_*`, `book/agents/cloth.md`).

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Isometry, Vec3, det_atan2};
use regia_ecs::spawn_order::SpawnKey;
use thiserror::Error;
use tracing::{debug_span, warn};

pub mod asset;
pub mod lod;
pub mod maps;
pub mod presets;
pub mod selfcol;
pub mod skin;
pub mod snapshot;
pub mod soft;
pub mod tuning;
mod vtcol;
pub mod wind;

pub use asset::{ClothAsset, ClothParams, ClothSource, SoftAsset, SoftSource};
pub use lod::{ClothState, LOD_OFF, LodLevel};
pub use maps::{MapEdit, apply_map_edit, transfer_maps};
pub use presets::{ClothPreset, GridEdge, PresetRow, UnknownPreset};
pub use skin::{PoseMaps, PoseTarget};
pub use snapshot::{SnapshotKind, SnapshotShape, WorldSnapshot};
pub use soft::{Plasticity, SoftModel, SoftParams, SoftSet, SoftSolver};
pub use tuning::{ClothTuning, SelfCollision, StrainCap, TearParams};
pub use wind::{Gust, MAX_WIND_SPEED, WindField, WindFieldKind, WindModel};

/// A full blend ramp: divisible by both [`lod::BLEND_IN_STEPS`] and [`lod::BLEND_OUT_STEPS`].
const RAMP_FULL: u32 = lod::BLEND_IN_STEPS * lod::BLEND_OUT_STEPS;

/// Version of the [`ClothSolver::state_digest`] encoding; part of the digest seed.
pub const DIGEST_VERSION: u32 = 2;

/// Most samples along a particle's relative path in a swept collider test.
pub const SWEEP_SAMPLES_MAX: u32 = 16;

/// Lengths and cross products below this are treated as zero (no direction to push along).
const EPSILON: f32 = 1e-9;

/// Most substeps per fixed step [`ClothConfig`] accepts.
pub const MAX_SUBSTEPS: u32 = 256;

/// Most fixed steps one `step_fixed` call (or `max_steps_per_call`) may run.
pub const MAX_STEP_FIXED_COUNT: u32 = 1024;

/// Most solver iterations per substep [`ClothConfig`] accepts.
pub const MAX_ITERATIONS: u32 = 64;

/// Most strain-cap sweeps after the last iteration of a substep.
pub const STRAIN_CAP_SWEEPS: u32 = 16;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Input a [`ClothSolver`] rejected. A rejected call changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClothError {
    /// A value was NaN or infinite.
    #[error("cloth: {what} is not finite")]
    NonFinite {
        /// What was not finite (`"dt"`, `"position"`, `"compliance"`, ...).
        what: &'static str,
    },
    /// A value that must be zero or positive was negative.
    #[error("cloth: {what} must not be negative")]
    Negative {
        /// What was negative (`"dt"`, `"inverse mass"`, `"radius"`, ...).
        what: &'static str,
    },
    /// A particle index does not exist.
    #[error("cloth: particle {index} is out of range ({len} particles)")]
    ParticleOutOfRange {
        /// The index given.
        index: usize,
        /// How many particles the solver has.
        len: usize,
    },
    /// A collider index does not exist.
    #[error("cloth: collider {index} is out of range ({len} colliders)")]
    ColliderOutOfRange {
        /// The index given.
        index: usize,
        /// How many colliders the solver has.
        len: usize,
    },
    /// A constraint names the same particle twice.
    #[error("cloth: a constraint names particle {index} twice")]
    RepeatedParticle {
        /// The repeated particle.
        index: usize,
    },
    /// A shape or builder description has no extent or direction.
    #[error("cloth: degenerate {what}")]
    Degenerate {
        /// What is degenerate (`"plane normal"`, `"grid size"`, `"rope length"`, ...).
        what: &'static str,
    },
    /// A [`ClothConfig`] count or step length is out of range.
    #[error("cloth: invalid config: {what}")]
    InvalidConfig {
        /// The offending field (`"substeps"`, `"iterations"`, `"time step"`, ...).
        what: &'static str,
    },
    /// A pose target list does not have one entry per particle.
    #[error("cloth: {len} pose targets for {expected} particles")]
    PoseSizeMismatch {
        /// Targets given.
        len: usize,
        /// Particles in the solver.
        expected: usize,
    },
    /// A blend outside `[0, 1]`.
    #[error("cloth: blend must be in [0, 1]")]
    BadBlend,
    /// A tuning, map or soft-body parameter is out of its documented range.
    #[error("cloth: parameter {what} is out of range")]
    BadParam {
        /// The parameter (`"inertia_scale"`, `"reach"`, `"pressure"`, ...).
        what: &'static str,
    },
    /// A vertex map does not have one value per particle.
    #[error("cloth: map {map} has {len} values for {expected} particles")]
    MapSizeMismatch {
        /// The map name.
        map: &'static str,
        /// Values given.
        len: usize,
        /// Particles in the solver.
        expected: usize,
    },
    /// A pressure soft body's surface is not a closed triangle mesh.
    #[error("soft body: the surface is not closed")]
    NotClosed,
    /// A tetrahedron has zero or negative volume at rest.
    #[error("soft body: tetrahedron {tet} is inverted or flat at rest")]
    InvertedRest {
        /// Index of the tetrahedron.
        tet: usize,
    },
    /// A world snapshot holds more triangles than `SNAPSHOT_TRIANGLES_MAX` or a hull with more
    /// planes than `SNAPSHOT_HULL_PLANES_MAX` (`cloth::snapshot`).
    #[error("cloth: snapshot has {len} {what}, over the limit")]
    SnapshotTooLarge {
        /// What is over the limit (`"triangles"`, `"hull planes"`).
        what: &'static str,
        /// How many there were.
        len: usize,
    },
    /// A shape-matching cluster has fewer than 4 particles, a repeated or unknown particle, or
    /// is flat.
    #[error("soft body: bad shape-matching cluster {cluster}")]
    BadCluster {
        /// Index of the cluster.
        cluster: usize,
    },
    /// A cloth or soft asset (source or cooked) is malformed (`cloth::asset`).
    #[error("cloth asset: {reason}")]
    AssetInvalid {
        /// What is wrong.
        reason: String,
    },
    /// A cloth or soft file is newer than this build reads.
    #[error("cloth asset: version {found} is newer than {supported}")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Newest version this build reads.
        supported: u32,
    },
}

impl ClothError {
    /// The stable, protocol-safe code of this error (lowercase ASCII, digits and `_`), as
    /// listed in `book/agents/cloth.md`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NonFinite { .. } => "cloth_non_finite",
            Self::Negative { .. } => "cloth_negative",
            Self::ParticleOutOfRange { .. } => "cloth_particle_out_of_range",
            Self::ColliderOutOfRange { .. } => "cloth_collider_out_of_range",
            Self::RepeatedParticle { .. } => "cloth_repeated_particle",
            Self::Degenerate { .. } => "cloth_degenerate",
            Self::InvalidConfig { .. } => "cloth_invalid_config",
            Self::PoseSizeMismatch { .. } => "cloth_pose_size_mismatch",
            Self::BadBlend => "cloth_bad_blend",
            Self::BadParam { .. } => "cloth_bad_param",
            Self::MapSizeMismatch { .. } => "cloth_map_size_mismatch",
            Self::NotClosed => "soft_not_closed",
            Self::InvertedRest { .. } => "soft_inverted_rest",
            Self::BadCluster { .. } => "soft_bad_cluster",
            Self::SnapshotTooLarge { .. } => "cloth_snapshot_too_large",
            Self::AssetInvalid { .. } => "cloth_asset_invalid",
            Self::UnsupportedVersion { .. } => "cloth_unsupported_version",
        }
    }
}

fn finite(value: f32, what: &'static str) -> Result<f32, ClothError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ClothError::NonFinite { what })
    }
}

fn finite_vec(value: Vec3, what: &'static str) -> Result<Vec3, ClothError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ClothError::NonFinite { what })
    }
}

fn non_negative(value: f32, what: &'static str) -> Result<f32, ClothError> {
    if finite(value, what)? < 0.0 {
        Err(ClothError::Negative { what })
    } else {
        Ok(value)
    }
}

// ---------------------------------------------------------------------------
// Configuration and data
// ---------------------------------------------------------------------------

/// Solver settings. Validated by [`ClothSolver::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothConfig {
    /// Acceleration applied to every free particle, in m/s² (default `(0, -9.81, 0)`).
    pub gravity: Vec3,
    /// Length of one fixed step in seconds (default 1/60). Must be positive.
    pub time_step: f32,
    /// Substeps per fixed step (default 8, at most [`MAX_SUBSTEPS`]). More substeps make stiff
    /// constraints converge better than more iterations do.
    pub substeps: u32,
    /// Gauss-Seidel passes per substep (default 1, at most [`MAX_ITERATIONS`]).
    pub iterations: u32,
    /// Linear velocity damping in 1/s (default 0.1): each substep scales velocities by
    /// `max(0, 1 - damping * h)`.
    pub damping: f32,
    /// Collision margin in meters (default 0.01): particles are kept this far outside every
    /// shape, which stands in for the cloth's thickness.
    pub thickness: f32,
    /// Most fixed steps one [`ClothSolver::step`] call runs (default 4); longer frames drop the
    /// excess time instead of spiraling.
    pub max_steps_per_call: u32,
}

impl Default for ClothConfig {
    fn default() -> Self {
        Self {
            gravity: Vec3::new(0.0, -9.81, 0.0),
            time_step: 1.0 / 60.0,
            substeps: 8,
            iterations: 1,
            damping: 0.1,
            thickness: 0.01,
            max_steps_per_call: 4,
        }
    }
}

impl ClothConfig {
    /// Checks every field: finite values, non-negative damping and thickness, a positive time
    /// step and counts in range.
    ///
    /// # Errors
    /// The first offending field as a [`ClothError`].
    pub fn validate(&self) -> Result<(), ClothError> {
        finite_vec(self.gravity, "gravity")?;
        if finite(self.time_step, "time step")? <= 0.0 {
            return Err(ClothError::InvalidConfig { what: "time step" });
        }
        if self.substeps == 0 || self.substeps > MAX_SUBSTEPS {
            return Err(ClothError::InvalidConfig { what: "substeps" });
        }
        // The solver divides by the squared substep length; a step so short that this overflows
        // fills the particles with NaN.
        let substep = self.time_step / self.substeps as f32;
        if !(1.0 / (substep * substep)).is_finite() {
            return Err(ClothError::InvalidConfig { what: "time step" });
        }
        if self.iterations == 0 || self.iterations > MAX_ITERATIONS {
            return Err(ClothError::InvalidConfig { what: "iterations" });
        }
        if self.max_steps_per_call == 0 || self.max_steps_per_call > MAX_STEP_FIXED_COUNT {
            return Err(ClothError::InvalidConfig {
                what: "max steps per call",
            });
        }
        non_negative(self.damping, "damping")?;
        non_negative(self.thickness, "thickness")?;
        Ok(())
    }
}

/// One simulated point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothParticle {
    /// World-space position in meters.
    pub position: Vec3,
    /// Velocity in m/s (always zero for pinned particles).
    pub velocity: Vec3,
    /// Inverse mass in 1/kg; `0` pins the particle (infinite mass, never moved by the solver).
    pub inv_mass: f32,
}

/// A collision shape particles are kept outside of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClothShape {
    /// A ball.
    Sphere {
        /// Center in world space.
        center: Vec3,
        /// Radius in meters (zero or more).
        radius: f32,
    },
    /// A segment `a`–`b` swept by a ball.
    Capsule {
        /// One end of the core segment.
        a: Vec3,
        /// The other end of the core segment (may equal `a`: a sphere).
        b: Vec3,
        /// Radius in meters (zero or more).
        radius: f32,
    },
    /// An infinite plane; particles stay on the side `normal · x >= offset`.
    Plane {
        /// Outward normal (any non-zero length; normalized when the collider is added).
        normal: Vec3,
        /// Signed distance of the plane from the origin along `normal`, in meters (measured
        /// along the normalized normal).
        offset: f32,
    },
}

/// A collision shape with its friction coefficient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothCollider {
    /// The shape.
    pub shape: ClothShape,
    /// Coulomb friction coefficient (zero or more): the tangential velocity a contact removes
    /// is at most `friction` times the normal velocity change the contact caused.
    pub friction: f32,
}

impl ClothCollider {
    /// Validates the collider and returns it with a unit plane normal.
    fn normalized(self) -> Result<Self, ClothError> {
        non_negative(self.friction, "friction")?;
        let shape = match self.shape {
            ClothShape::Sphere { center, radius } => ClothShape::Sphere {
                center: finite_vec(center, "sphere center")?,
                radius: non_negative(radius, "radius")?,
            },
            ClothShape::Capsule { a, b, radius } => ClothShape::Capsule {
                a: finite_vec(a, "capsule end")?,
                b: finite_vec(b, "capsule end")?,
                radius: non_negative(radius, "radius")?,
            },
            ClothShape::Plane { normal, offset } => {
                let normal = finite_vec(normal, "plane normal")?;
                let offset = finite(offset, "plane offset")?;
                let length = normal.length();
                if !(length > EPSILON && length.is_finite()) {
                    return Err(ClothError::Degenerate { what: "plane normal" });
                }
                ClothShape::Plane {
                    normal: normal / length,
                    offset,
                }
            }
        };
        Ok(Self {
            shape,
            friction: self.friction,
        })
    }

    /// Signed distance from `x` to the shape's surface and the outward unit normal there.
    fn signed_distance(&self, x: Vec3) -> (f32, Vec3) {
        match self.shape {
            ClothShape::Sphere { center, radius } => sphere_distance(x, center, radius),
            ClothShape::Capsule { a, b, radius } => {
                let ab = b - a;
                let len2 = ab.length_squared();
                let t = if len2 > EPSILON {
                    ((x - a).dot(ab) / len2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                sphere_distance(x, a + ab * t, radius)
            }
            ClothShape::Plane { normal, offset } => (normal.dot(x) - offset, normal),
        }
    }
}

/// Distance to a ball; a point at the exact center is pushed out along +Y (any fixed choice
/// keeps runs deterministic).
fn sphere_distance(x: Vec3, center: Vec3, radius: f32) -> (f32, Vec3) {
    let d = x - center;
    let len = d.length();
    if len > EPSILON {
        (len - radius, d / len)
    } else {
        (-radius, Vec3::Y)
    }
}

#[derive(Debug, Clone, Copy)]
struct DistanceConstraint {
    a: usize,
    b: usize,
    rest: f32,
    compliance: f32,
    lambda: f32,
    /// `false` once torn (spec 55 § 2.5).
    active: bool,
}

/// Dihedral bending across the shared edge `a`–`b` of triangles `(a, b, c)` and `(a, b, d)`.
#[derive(Debug, Clone, Copy)]
struct BendConstraint {
    ids: [usize; 4],
    rest_angle: f32,
    compliance: f32,
    lambda: f32,
}

/// Per-particle contact bookkeeping for one substep (friction input).
#[derive(Debug, Clone, Copy, Default)]
struct Contact {
    /// Total distance the collision projection moved the particle this substep.
    depth: f32,
    /// Normal of the last contact.
    normal: Vec3,
    /// Friction coefficient of the last contact.
    friction: f32,
    /// Velocity of the collider's surface at the last contact (zero for static colliders).
    velocity: Vec3,
}

/// Counts and measures of a [`ClothSolver`] (agent `physics.cloth_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClothStats {
    /// Particles, pinned ones included.
    pub particles: usize,
    /// Particles with inverse mass 0.
    pub pinned: usize,
    /// Distance constraints (stretch, shear and rope bending links).
    pub distance_constraints: usize,
    /// Dihedral bending constraints.
    pub bend_constraints: usize,
    /// Collision shapes.
    pub colliders: usize,
    /// Fixed steps run since the solver was created.
    pub steps: u64,
    /// Fixed steps skipped because a frame needed more than `max_steps_per_call`.
    pub dropped_steps: u64,
    /// Particles touching a shape in the last substep.
    pub contacts: usize,
    /// Largest `current length / rest length` over the distance constraints with a non-zero
    /// rest length (1 = unstretched; 0 when there are none).
    pub max_stretch: f32,
}

/// Particles and constraints a [`ClothSolver::add_grid`] call created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClothGridHandle {
    /// Index of the particle at column 0, row 0. Particles are stored row by row.
    pub first: usize,
    /// Particles per row.
    pub columns: usize,
    /// Rows.
    pub rows: usize,
}

impl ClothGridHandle {
    /// Index of the particle at `column`, `row`, or `None` outside the grid.
    #[must_use]
    pub fn index(&self, column: usize, row: usize) -> Option<usize> {
        (column < self.columns && row < self.rows).then(|| self.first + row * self.columns + column)
    }
}

/// Particles a [`ClothSolver::add_rope`] call created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClothRopeHandle {
    /// Index of the particle at the rope's start.
    pub first: usize,
    /// Particles in the rope (`segments + 1`), stored from start to end.
    pub count: usize,
}

/// A rectangular cloth for [`ClothSolver::add_grid`]: `columns × rows` particles at
/// `origin + u_axis * column * spacing + v_axis * row * spacing`.
///
/// Each quad is split into two triangles along its `(c, r)`–`(c + 1, r + 1)` diagonal. The
/// triangle edges get distance constraints with `stretch_compliance`, the other diagonal one
/// with `shear_compliance`, and every interior triangle edge a dihedral bending constraint
/// with `bend_compliance`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothGrid {
    /// Position of the particle at column 0, row 0.
    pub origin: Vec3,
    /// Direction of increasing column (normalized by the builder).
    pub u_axis: Vec3,
    /// Direction of increasing row (normalized by the builder; must not be parallel to
    /// `u_axis`).
    pub v_axis: Vec3,
    /// Particles per row (at least 2).
    pub columns: usize,
    /// Rows (at least 2).
    pub rows: usize,
    /// Distance between neighboring particles in meters (positive).
    pub spacing: f32,
    /// Mass of each particle in kilograms; `0` makes every particle pinned.
    pub particle_mass: f32,
    /// Compliance of the structural (grid-line) and triangle-diagonal links.
    pub stretch_compliance: f32,
    /// Compliance of the second diagonal of each quad.
    pub shear_compliance: f32,
    /// Compliance of the dihedral bending constraints.
    pub bend_compliance: f32,
}

/// A rope for [`ClothSolver::add_rope`]: `segments + 1` evenly spaced particles from `start`
/// to `end`, joined by stretch links, with a link from each particle to the one after next for
/// bending stiffness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothRope {
    /// First particle.
    pub start: Vec3,
    /// Last particle (must differ from `start`).
    pub end: Vec3,
    /// Number of segments (at least 1).
    pub segments: usize,
    /// Mass of each particle in kilograms; `0` makes every particle pinned.
    pub particle_mass: f32,
    /// Compliance of the links between neighbors.
    pub stretch_compliance: f32,
    /// Compliance of the skip-one bending links.
    pub bend_compliance: f32,
}

fn inv_mass_of(mass: f32) -> Result<f32, ClothError> {
    let mass = non_negative(mass, "particle mass")?;
    let inv = if mass > 0.0 { 1.0 / mass } else { 0.0 };
    // A subnormal mass overflows to infinity.
    finite(inv, "inverse mass")
}

// ---------------------------------------------------------------------------
// Solver
// ---------------------------------------------------------------------------

/// A tether: free particle `particle` stays within `scale * length` of `anchor` (spec 55 § 2.3,
/// long-range attachments).
#[derive(Debug, Clone, Copy)]
struct Tether {
    particle: usize,
    anchor: usize,
    /// Rest path length along the distance links.
    length: f32,
}

/// A distance link cut by tearing (spec 55 § 2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClothTorn {
    /// Index of the distance constraint (insertion order).
    pub constraint: usize,
    /// The solver's fixed-step count when it was cut.
    pub step: u64,
}

/// One snapshot shape's share of the two-way reaction since the last
/// [`ClothSolver::take_reaction_wrench`] (spec 55 § 2.4, ADR-2425).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothReaction {
    /// The snapshot shape's key (the collider's spawn-arrival key).
    pub key: SpawnKey,
    /// Linear impulse the shape received (N·s, world space).
    pub impulse: Vec3,
    /// Angular impulse the shape received about [`ClothReaction::about`] (N·m·s, world space).
    pub angular_impulse: Vec3,
    /// The point the angular impulse is taken about: the shape's current origin.
    pub about: Vec3,
}

impl ClothReaction {
    /// The angular impulse about `point` (a body's center of mass):
    /// `angular_impulse + (about - point) × impulse`.
    #[must_use]
    pub fn angular_about(&self, point: Vec3) -> Vec3 {
        self.angular_impulse + (self.about - point).cross(self.impulse)
    }
}

/// Spec 55 state of a [`ClothSolver`] beyond [`ClothStats`] (agent `physics.cloth_inspect`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothDiagnostics {
    /// Lifecycle state.
    pub state: ClothState,
    /// The blend the output uses: the game's blend times the state ramp.
    pub blend: f32,
    /// Distance LOD level (0..=2, or [`LOD_OFF`]).
    pub lod: LodLevel,
    /// Self-collision pairs over the per-substep cap, summed since the solver was made.
    pub dropped_self_pairs: u64,
    /// Self-collision pairs in contact in the last substep.
    pub self_contacts: usize,
    /// Distance links cut by tearing.
    pub torn: usize,
    /// Wind velocity the solver used in its last step (m/s).
    pub wind: Vec3,
    /// Largest distance from a free bound particle to its pose target (0 without a pose).
    pub max_pose_error: f32,
    /// Teleport resets since the solver was made.
    pub teleports: u64,
    /// Tethers in use.
    pub tethers: usize,
}

/// Default [`ClothGuard::max_strain`]: a link ten times its rest length is far past any fabric,
/// so a sheet over it has blown up rather than stretched (ADR-3273).
pub const DEFAULT_GUARD_STRAIN: f32 = 10.0;

/// Default [`ClothGuard::stiff_limit`]: a zero-compliance link may be 7 % longer than its rest
/// length when the sweep reaches it before the solver steps in (ADR-3273). The smallest limit
/// the spec 55 test 1 sheet never reaches, so its pinned digest is unchanged.
pub const DEFAULT_STIFF_LIMIT: f32 = 1.07;

/// Most speed one stiff-limit correction (an attachment or a link) adds to a particle, in m/s
/// (ADR-3273): the excess of a longer move (a sheet hauled after its pin was teleported) adds
/// none. Per correction, not per particle: capping a particle's summed move kept the full limit
/// in the move's direction while dropping the sweeps' opposing corrections, and whipped a 6 m
/// sheet to 66 m/s.
pub const ATTACHMENT_SPEED_MAX: f32 = 20.0;

/// Default [`ClothGuard::stiff_target`]: pulled back to 2 % over rest length (all the way to 1
/// fights the links the sweep just solved and ends longer, ADR-3273).
pub const DEFAULT_STIFF_TARGET: f32 = 1.02;

/// Default [`ClothGuard::extra_iterations`]: none, the projection alone held the spec 55 § 10
/// sheet under 1.1x at no iteration cost (ADR-3273).
pub const DEFAULT_EXTRA_ITERATIONS: u32 = 0;

/// Most [`ClothFault`] events a solver keeps until [`ClothSolver::drain_faults`]; older ones are
/// still counted in [`ClothHealth`].
pub const FAULT_EVENTS_MAX: usize = 16;

/// The runaway guard of a [`ClothSolver`] (ADR-3273), set with [`ClothSolver::set_guard`].
///
/// **Prevention** (`stiff_limit`). One Gauss-Seidel sweep per substep cannot carry the load of a
/// tall or heavy sheet: its zero-compliance (inextensible) links stay stretched, and the sweep's
/// order-dependent residual next to pinned particles turns into kinetic energy until the sheet
/// blows up. So when a sweep finds a zero-compliance link longer than `stiff_limit` times its
/// rest length, the whole iteration (every constraint, collisions last) may run again, at most
/// `extra_iterations` more times per substep; if the last sweep still found one, every free
/// particle is pulled back within `stiff_target` times its rest path length from its nearest
/// pinned particle (long-range attachments), the stiff links are projected back to
/// `stiff_target` times their rest length (split by inverse mass) and the colliders run again.
/// The attachments follow zero-compliance links only, and no correction of the pass adds more
/// than [`ATTACHMENT_SPEED_MAX`] of speed, so a sheet hauled after a teleported pin follows
/// without whipping. A solver whose stiff links stay within the limit
/// runs exactly the authored iterations and is bit-identical to a solver without the limit.
///
/// **Detection and recovery** (`enabled`, `max_strain`). At the end of every fixed step the
/// guard checks that every particle's position and velocity is finite and that no active
/// zero-compliance distance link is longer than `max_strain` times its rest length (compliant
/// links may stretch past it; an elastic cloth still stretches, ADR-3273). Tearing, when
/// enabled, runs only after the guard passed, so a faulted step is undone wholesale and never
/// leaves the cloth torn. A step that fails is a fault: the solver goes back to the last step
/// that passed (one deep, velocities zeroed; pins keep where the caller put them, and two-way
/// reaction impulses of the step are dropped) or, when the fault follows another one or there
/// is no such step (none since a reset), to its rest or skinned pose
/// ([`ClothSolver::reset_to_pose`]), and records a [`ClothFault`]. A stiff link over
/// `max_strain` is no fault under the moved-pin excuse: its own baseline — the last good
/// step's free particles against that link's pins' current places — is over `max_strain` too
/// (a caller parked a pin far away, and the solver is pulling the cloth after it). A link
/// whose baseline is within the limit still faults no matter how far apart an unrelated pin
/// pair sits: the cloth then follows the pin where it can, and the guard holds the rest. A
/// step that passes leaves the state bit for bit as the solver made it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothGuard {
    /// Whether fault detection and recovery run (default `true`). Off, nothing is checked or
    /// copied (the `stiff_limit` prevention is separate).
    pub enabled: bool,
    /// Largest accepted `length / rest length` of an active zero-compliance link at the end of
    /// a fixed step (default [`DEFAULT_GUARD_STRAIN`]; finite and greater than 1). Compliant
    /// links may stretch past it without a fault.
    pub max_strain: f32,
    /// Stretch ratio of zero-compliance links above which a substep iterates again (default
    /// [`DEFAULT_STIFF_LIMIT`]; finite and greater than 1), or `None` for exactly the authored
    /// iterations and no stiff-link projection.
    pub stiff_limit: Option<f32>,
    /// Most extra iterations per substep the `stiff_limit` may add (default
    /// [`DEFAULT_EXTRA_ITERATIONS`], at most [`MAX_ITERATIONS`]; 0 leaves only the projection).
    pub extra_iterations: u32,
    /// Stretch ratio the projection pulls zero-compliance links and tethers back to once the
    /// `stiff_limit` fired (default [`DEFAULT_STIFF_TARGET`]; at least 1, at most `stiff_limit`).
    pub stiff_target: f32,
}

impl Default for ClothGuard {
    fn default() -> Self {
        Self {
            enabled: true,
            max_strain: DEFAULT_GUARD_STRAIN,
            stiff_limit: Some(DEFAULT_STIFF_LIMIT),
            extra_iterations: DEFAULT_EXTRA_ITERATIONS,
            stiff_target: DEFAULT_STIFF_TARGET,
        }
    }
}

impl ClothGuard {
    /// Checks every field.
    ///
    /// # Errors
    /// [`ClothError::BadParam`] (`"max_strain"`, `"stiff_limit"`, `"stiff_target"`,
    /// `"extra_iterations"`) when `max_strain` or `stiff_limit` is not finite or not above 1,
    /// `stiff_target` is not in `1..=stiff_limit`, or `extra_iterations` is over
    /// [`MAX_ITERATIONS`].
    pub fn validate(&self) -> Result<(), ClothError> {
        let ratio = |v: f32| v.is_finite() && v > 1.0;
        if !ratio(self.max_strain) {
            return Err(ClothError::BadParam { what: "max_strain" });
        }
        if self.stiff_limit.is_some_and(|l| !ratio(l)) {
            return Err(ClothError::BadParam { what: "stiff_limit" });
        }
        let limit = self.stiff_limit.unwrap_or(f32::INFINITY);
        if !(self.stiff_target.is_finite() && self.stiff_target >= 1.0 && self.stiff_target <= limit) {
            return Err(ClothError::BadParam { what: "stiff_target" });
        }
        if self.extra_iterations > MAX_ITERATIONS {
            return Err(ClothError::BadParam {
                what: "extra_iterations",
            });
        }
        Ok(())
    }
}

/// What tripped the [`ClothGuard`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClothFaultKind {
    /// A particle position or velocity was NaN or infinite.
    NonFinite,
    /// An active zero-compliance link was longer than [`ClothGuard::max_strain`] times its rest
    /// length, and worse than its own moved-pin baseline (see [`ClothGuard`]).
    Runaway,
}

impl ClothFaultKind {
    /// The stable protocol code (`cloth_fault_non_finite`, `cloth_fault_runaway`), as listed in
    /// `book/agents/cloth.md`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NonFinite => "cloth_fault_non_finite",
            Self::Runaway => "cloth_fault_runaway",
        }
    }
}

/// Where a faulted solver was put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClothRecovery {
    /// The particles of the last fixed step that passed the guard, velocities zeroed.
    LastGood,
    /// The rest or skinned pose, as [`ClothSolver::reset_to_pose`] (state `Settling`).
    RestPose,
}

impl ClothRecovery {
    /// Stable lowercase name (`last_good`, `rest_pose`) for the agent protocol.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::LastGood => "last_good",
            Self::RestPose => "rest_pose",
        }
    }
}

/// One guard fault (ADR-3273), drained with [`ClothSolver::drain_faults`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothFault {
    /// The solver's fixed-step count of the step that faulted (before it is counted).
    pub step: u64,
    /// What tripped the guard.
    pub kind: ClothFaultKind,
    /// Where the solver was put back.
    pub recovery: ClothRecovery,
    /// The first particle with a non-finite position or velocity ([`ClothFaultKind::NonFinite`]).
    pub particle: Option<usize>,
    /// Largest finite `length / rest length` of an active zero-compliance link when the step
    /// ended (0 when none was finite).
    pub strain: f32,
}

impl ClothFault {
    /// The protocol code of the fault ([`ClothFaultKind::code`]).
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

/// Guard counters and strain of a [`ClothSolver`] (agent `physics.cloth_health`, ADR-3273).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClothHealth {
    /// The guard settings in use.
    pub guard: ClothGuard,
    /// Largest `length / rest length` of an active zero-compliance link at the end of the last
    /// checked fixed step (after any recovery; 0 before the first step or with the guard off).
    /// Compliant links are never counted (an elastic cloth may stretch past any ratio).
    pub max_strain: f32,
    /// Largest `max_strain` seen at the end of a step that passed the guard, since the solver
    /// was made.
    pub peak_strain: f32,
    /// Faults since the solver was made.
    pub faults: u64,
    /// Of those, [`ClothFaultKind::NonFinite`] ones.
    pub non_finite_faults: u64,
    /// Of those, [`ClothFaultKind::Runaway`] ones.
    pub runaway_faults: u64,
    /// Of those, the ones recovered to the rest or skinned pose.
    pub pose_resets: u64,
    /// The most recent fault.
    pub last_fault: Option<ClothFault>,
    /// Extra iterations the `stiff_limit` ran, summed since the solver was made.
    pub extra_iterations: u64,
    /// Substeps that ended with the stiff-link projection, since the solver was made.
    pub stiff_projections: u64,
}

/// Guard state of a solver (ADR-3273).
#[derive(Debug, Clone, Default)]
struct GuardState {
    settings: ClothGuard,
    /// Particles at the end of the last fixed step that passed (empty: none yet).
    good: Vec<ClothParticle>,
    /// Faults in a row; a fault right after another one goes to the pose.
    streak: u32,
    max_strain: f32,
    peak_strain: f32,
    faults: u64,
    non_finite: u64,
    runaway: u64,
    pose_resets: u64,
    last: Option<ClothFault>,
    events: Vec<ClothFault>,
    extra_iterations: u64,
    stiff_projections: u64,
    /// Long-range attachments to the pinned particles at the stiff limit (built on first use
    /// after a topology or pin change).
    tethers: Vec<Tether>,
    tethers_valid: bool,
    /// Two-way reaction sums at the start of the fixed step, restored when the step faults.
    reaction: Vec<Vec3>,
    reaction_angular: Vec<Vec3>,
}

/// The XPBD cloth and rope solver. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct ClothSolver {
    config: ClothConfig,
    particles: Vec<ClothParticle>,
    /// Positions at the start of the current substep.
    previous: Vec<Vec3>,
    /// Positions when each particle was added (reset target without a pose).
    rest: Vec<Vec3>,
    contacts: Vec<Contact>,
    distance: Vec<DistanceConstraint>,
    bends: Vec<BendConstraint>,
    colliders: Vec<ClothCollider>,
    /// Previous shapes for this fixed step (`set_collider_previous`), consumed by the step.
    collider_prev: Vec<Option<ClothCollider>>,
    /// Scratch: collider shapes at the start and end of the running substep.
    collider_from: Vec<ClothCollider>,
    collider_to: Vec<ClothCollider>,
    snapshot: WorldSnapshot,
    /// Two-way reaction: the per-body impulse cap when on (`set_reaction`).
    reaction_cap: Option<f32>,
    /// Impulse each snapshot shape received since the last `take_reaction` (N·s).
    reaction: Vec<Vec3>,
    /// Angular impulse about each snapshot shape's current origin, same period (N·m·s).
    reaction_angular: Vec<Vec3>,
    triangles: Vec<[usize; 3]>,
    hidden: Vec<bool>,
    tuning: ClothTuning,
    pose: skin::PoseBinding,
    tethers: Vec<Tether>,
    tethers_valid: bool,
    selfcol: selfcol::SelfCollider,
    vtcol: vtcol::VertexTriangleCollider,
    /// Positions at the start of the running fixed step (vertex-triangle side reference).
    step_start: Vec<Vec3>,
    /// Garment layer per particle (empty: every particle on layer 0).
    layers: Vec<u8>,
    /// Extra contact distance across layers; `None` is twice the self-collision radius.
    layer_gap: Option<f32>,
    wind: Vec3,
    gust: Option<Gust>,
    field_wind: Vec3,
    global_wind: Vec3,
    last_wind: Vec3,
    wind_accel: Vec<Vec3>,
    state: ClothState,
    state_steps: u32,
    /// Blend ramp in units of 1/[`RAMP_FULL`] (integer, so a full ramp is exactly 1).
    ramp: u32,
    blend: f32,
    lod: LodLevel,
    paused_steps: u32,
    torn: usize,
    torn_events: Vec<ClothTorn>,
    teleports: u64,
    /// Soft-body model ([`SoftSolver`]); `None` for cloth and ropes.
    soft: Option<Box<soft::SoftState>>,
    accumulator: f32,
    steps: u64,
    dropped_steps: u64,
    last_contacts: usize,
    /// Runaway guard (ADR-3273).
    guard: GuardState,
}

impl ClothSolver {
    /// An empty solver.
    ///
    /// # Errors
    /// [`ClothConfig::validate`]'s error when a setting is out of range.
    pub fn new(config: ClothConfig) -> Result<Self, ClothError> {
        config.validate()?;
        Ok(Self {
            config,
            particles: Vec::new(),
            previous: Vec::new(),
            rest: Vec::new(),
            contacts: Vec::new(),
            distance: Vec::new(),
            bends: Vec::new(),
            colliders: Vec::new(),
            collider_prev: Vec::new(),
            collider_from: Vec::new(),
            collider_to: Vec::new(),
            snapshot: WorldSnapshot::default(),
            reaction_cap: None,
            reaction: Vec::new(),
            reaction_angular: Vec::new(),
            triangles: Vec::new(),
            hidden: Vec::new(),
            tuning: ClothTuning::default(),
            pose: skin::PoseBinding::default(),
            tethers: Vec::new(),
            tethers_valid: false,
            selfcol: selfcol::SelfCollider::default(),
            vtcol: vtcol::VertexTriangleCollider::default(),
            step_start: Vec::new(),
            layers: Vec::new(),
            layer_gap: None,
            wind: Vec3::ZERO,
            gust: None,
            field_wind: Vec3::ZERO,
            global_wind: Vec3::ZERO,
            last_wind: Vec3::ZERO,
            wind_accel: Vec::new(),
            state: ClothState::Running,
            state_steps: 0,
            ramp: RAMP_FULL,
            blend: 1.0,
            lod: 0,
            paused_steps: 0,
            torn: 0,
            torn_events: Vec::new(),
            teleports: 0,
            soft: None,
            accumulator: 0.0,
            steps: 0,
            dropped_steps: 0,
            last_contacts: 0,
            guard: GuardState::default(),
        })
    }

    /// The settings the solver runs with.
    #[must_use]
    pub fn config(&self) -> &ClothConfig {
        &self.config
    }

    /// Re-expresses every world-space position after an origin rebase: `shift` is subtracted from
    /// particles, their previous and rest positions, the collision shapes (current and previous) and
    /// the rigid snapshot (spec 78 § 12.2). Velocities are differences, so they are unchanged.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for particle in &mut self.particles {
            particle.position -= shift;
        }
        for point in self
            .previous
            .iter_mut()
            .chain(self.rest.iter_mut())
            .chain(self.step_start.iter_mut())
        {
            *point -= shift;
        }
        for good in &mut self.guard.good {
            good.position -= shift;
        }
        let shift_shape = |collider: &mut ClothCollider| match &mut collider.shape {
            ClothShape::Sphere { center, .. } => *center -= shift,
            ClothShape::Capsule { a, b, .. } => {
                *a -= shift;
                *b -= shift;
            }
            ClothShape::Plane { normal, offset } => *offset -= normal.dot(shift),
        };
        self.colliders.iter_mut().for_each(shift_shape);
        self.collider_prev.iter_mut().flatten().for_each(shift_shape);
        self.snapshot.shift_origin(shift);
        // The pose targets and the root motion are world-space too: left behind they would drag
        // the cloth back by the whole shift on the next step. Both roots move by the same
        // amount, so the teleport test sees the same jump.
        self.pose.shift_origin(shift);
    }

    /// Every particle, in insertion order (the index is the particle's id).
    #[must_use]
    pub fn particles(&self) -> &[ClothParticle] {
        &self.particles
    }

    /// Every collider, in insertion order.
    #[must_use]
    pub fn colliders(&self) -> &[ClothCollider] {
        &self.colliders
    }

    /// Adds a particle at rest and returns its index. `inv_mass` is `1 / mass`; `0` pins it.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] for a NaN or infinite position or inverse mass,
    /// [`ClothError::Negative`] for a negative inverse mass.
    pub fn add_particle(&mut self, position: Vec3, inv_mass: f32) -> Result<usize, ClothError> {
        let position = finite_vec(position, "position")?;
        let inv_mass = non_negative(inv_mass, "inverse mass")?;
        self.particles.push(ClothParticle {
            position,
            velocity: Vec3::ZERO,
            inv_mass,
        });
        self.previous.push(position);
        self.rest.push(position);
        self.contacts.push(Contact::default());
        self.wind_accel.push(Vec3::ZERO);
        if self.pose.maps.is_some() {
            self.pose.base_inv_mass.push(inv_mass);
            self.pose.pull_lambda.push(0.0);
        }
        if self.pose.has_targets() {
            // Keep the per-particle target arrays the same length as the particles: the new
            // particle's target is its own rest position with no normal.
            let target = PoseTarget {
                position,
                normal: Vec3::ZERO,
            };
            self.pose.prev.push(target);
            self.pose.cur.push(target);
            if !self.pose.sub.is_empty() {
                self.pose.sub.push(target);
            }
        }
        self.topology_changed();
        Ok(self.particles.len() - 1)
    }

    /// Marks the derived data (tethers, self-collision exclusions) stale.
    fn topology_changed(&mut self) {
        self.tethers_valid = false;
        self.selfcol.invalidate();
        // A last-good copy of another topology is no state to go back to (ADR-3273).
        self.guard.good.clear();
    }

    fn check_particle(&self, index: usize) -> Result<(), ClothError> {
        if index < self.particles.len() {
            Ok(())
        } else {
            Err(ClothError::ParticleOutOfRange {
                index,
                len: self.particles.len(),
            })
        }
    }

    fn check_distinct(&self, ids: &[usize]) -> Result<(), ClothError> {
        for (i, &id) in ids.iter().enumerate() {
            self.check_particle(id)?;
            if ids[..i].contains(&id) {
                return Err(ClothError::RepeatedParticle { index: id });
            }
        }
        Ok(())
    }

    /// Adds a distance constraint between particles `a` and `b` whose rest length is their
    /// current distance, and returns its index among the distance constraints.
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::RepeatedParticle`] (`a == b`), and
    /// [`ClothError::NonFinite`] / [`ClothError::Negative`] for the compliance.
    pub fn add_distance(&mut self, a: usize, b: usize, compliance: f32) -> Result<usize, ClothError> {
        self.check_distinct(&[a, b])?;
        let compliance = non_negative(compliance, "compliance")?;
        let rest = (self.particles[a].position - self.particles[b].position).length();
        self.distance.push(DistanceConstraint {
            a,
            b,
            rest,
            compliance,
            lambda: 0.0,
            active: true,
        });
        self.topology_changed();
        Ok(self.distance.len() - 1)
    }

    /// Adds a dihedral bending constraint across the edge `a`–`b` shared by the triangles
    /// `(a, b, c)` and `(a, b, d)`, with the current angle between them as the rest angle, and
    /// returns its index among the bending constraints.
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::RepeatedParticle`], and
    /// [`ClothError::NonFinite`] / [`ClothError::Negative`] for the compliance.
    pub fn add_bend(
        &mut self,
        a: usize,
        b: usize,
        c: usize,
        d: usize,
        compliance: f32,
    ) -> Result<usize, ClothError> {
        let ids = [a, b, c, d];
        self.check_distinct(&ids)?;
        let compliance = non_negative(compliance, "compliance")?;
        let rest_angle = dihedral(self.positions_of(ids)).map_or(0.0, |g| g.angle);
        self.bends.push(BendConstraint {
            ids,
            rest_angle,
            compliance,
            lambda: 0.0,
        });
        self.topology_changed();
        Ok(self.bends.len() - 1)
    }

    fn positions_of(&self, ids: [usize; 4]) -> [Vec3; 4] {
        ids.map(|i| self.particles[i].position)
    }

    /// Pins particle `index`: inverse mass 0 and zero velocity. Move it with
    /// [`ClothSolver::set_position`].
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`].
    pub fn pin(&mut self, index: usize) -> Result<(), ClothError> {
        self.set_inv_mass(index, 0.0)
    }

    /// Sets particle `index`'s inverse mass (`0` pins it and zeroes its velocity).
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::NonFinite`], [`ClothError::Negative`].
    pub fn set_inv_mass(&mut self, index: usize, inv_mass: f32) -> Result<(), ClothError> {
        self.check_particle(index)?;
        let inv_mass = non_negative(inv_mass, "inverse mass")?;
        if let Some(base) = self.pose.base_inv_mass.get_mut(index) {
            *base = inv_mass;
        }
        let effective = self.pose.effective_inv_mass(index, inv_mass);
        let particle = &mut self.particles[index];
        particle.inv_mass = effective;
        if effective == 0.0 {
            particle.velocity = Vec3::ZERO;
        }
        self.tethers_valid = false;
        Ok(())
    }

    /// Moves particle `index` (typically a pinned attachment point) without changing its
    /// velocity. Constraints pull the free particles after it on the next step.
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::NonFinite`].
    pub fn set_position(&mut self, index: usize, position: Vec3) -> Result<(), ClothError> {
        self.check_particle(index)?;
        self.particles[index].position = finite_vec(position, "position")?;
        Ok(())
    }

    /// Sets particle `index`'s velocity. Ignored (kept zero) for pinned particles.
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::NonFinite`].
    pub fn set_velocity(&mut self, index: usize, velocity: Vec3) -> Result<(), ClothError> {
        self.check_particle(index)?;
        let velocity = finite_vec(velocity, "velocity")?;
        let particle = &mut self.particles[index];
        if particle.inv_mass > 0.0 {
            particle.velocity = velocity;
        }
        Ok(())
    }

    /// Adds a collision shape and returns its index. Plane normals are normalized.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] for any NaN or infinite value, [`ClothError::Negative`] for a
    /// negative radius or friction, [`ClothError::Degenerate`] for a zero plane normal.
    pub fn add_collider(&mut self, collider: ClothCollider) -> Result<usize, ClothError> {
        self.colliders.push(collider.normalized()?);
        self.collider_prev.push(None);
        Ok(self.colliders.len() - 1)
    }

    /// Replaces collider `index` (to move or resize it between steps).
    ///
    /// # Errors
    /// [`ClothError::ColliderOutOfRange`] and the errors of [`ClothSolver::add_collider`].
    pub fn set_collider(&mut self, index: usize, collider: ClothCollider) -> Result<(), ClothError> {
        let len = self.colliders.len();
        let collider = collider.normalized();
        let slot = self
            .colliders
            .get_mut(index)
            .ok_or(ClothError::ColliderOutOfRange { index, len })?;
        *slot = collider?;
        Ok(())
    }

    /// Builds a rectangular cloth (see [`ClothGrid`]). Nothing is added when the description is
    /// rejected.
    ///
    /// # Errors
    /// [`ClothError::Degenerate`] for fewer than 2 columns or rows, a non-positive spacing, a
    /// zero or parallel axis, or a grid whose particle count overflows; [`ClothError::NonFinite`]
    /// / [`ClothError::Negative`] for bad numbers.
    pub fn add_grid(&mut self, grid: &ClothGrid) -> Result<ClothGridHandle, ClothError> {
        let origin = finite_vec(grid.origin, "grid origin")?;
        let u = finite_vec(grid.u_axis, "grid axis")?;
        let v = finite_vec(grid.v_axis, "grid axis")?;
        let spacing = finite(grid.spacing, "grid spacing")?;
        let inv_mass = inv_mass_of(grid.particle_mass)?;
        let stretch = non_negative(grid.stretch_compliance, "compliance")?;
        let shear = non_negative(grid.shear_compliance, "compliance")?;
        let bend = non_negative(grid.bend_compliance, "compliance")?;
        let (columns, rows) = (grid.columns, grid.rows);
        if columns < 2
            || rows < 2
            || columns
                .checked_mul(rows)
                .is_none_or(|n| n > asset::MAX_ASSET_PARTICLES)
        {
            return Err(ClothError::Degenerate { what: "grid size" });
        }
        if spacing <= 0.0 {
            return Err(ClothError::Degenerate { what: "grid spacing" });
        }
        let (u, v) = (u.normalize_or_zero(), v.normalize_or_zero());
        if u == Vec3::ZERO || v == Vec3::ZERO || u.cross(v).length() <= 1e-6 {
            return Err(ClothError::Degenerate { what: "grid axis" });
        }

        let first = self.particles.len();
        let handle = ClothGridHandle { first, columns, rows };
        for row in 0..rows {
            for column in 0..columns {
                let p = origin + u * (column as f32 * spacing) + v * (row as f32 * spacing);
                if let Err(err) = self.add_particle(p, inv_mass) {
                    // Only reachable when a huge grid overflows f32: undo the partial grid.
                    self.truncate_particles(first);
                    return Err(err);
                }
            }
        }
        let at = |column: usize, row: usize| first + row * columns + column;

        // Triangles (two per quad, split along the (c, r)-(c+1, r+1) diagonal) and the
        // distance links. Indices are valid and distinct by construction.
        let mut triangles = Vec::with_capacity(2 * (columns - 1) * (rows - 1)); // columns * rows is capped above
        for row in 0..rows {
            for column in 0..columns {
                if column + 1 < columns {
                    self.push_distance(at(column, row), at(column + 1, row), stretch);
                }
                if row + 1 < rows {
                    self.push_distance(at(column, row), at(column, row + 1), stretch);
                }
                if column + 1 < columns && row + 1 < rows {
                    let (p00, p10) = (at(column, row), at(column + 1, row));
                    let (p01, p11) = (at(column, row + 1), at(column + 1, row + 1));
                    self.push_distance(p00, p11, stretch);
                    self.push_distance(p10, p01, shear);
                    triangles.push([p00, p10, p11]);
                    triangles.push([p00, p11, p01]);
                    self.triangles.push([p00, p10, p11]);
                    self.triangles.push([p00, p11, p01]);
                    self.hidden.extend([false, false]);
                }
            }
        }

        // Every edge shared by two triangles gets a bending constraint; a BTreeMap keeps the
        // order deterministic.
        let mut wings: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
        for tri in &triangles {
            for k in 0..3 {
                let (a, b, opposite) = (tri[k], tri[(k + 1) % 3], tri[(k + 2) % 3]);
                wings.entry((a.min(b), a.max(b))).or_default().push(opposite);
            }
        }
        for ((a, b), opposite) in wings {
            if let [c, d] = opposite[..] {
                let ids = [a, b, c, d];
                let rest_angle = dihedral(self.positions_of(ids)).map_or(0.0, |g| g.angle);
                self.bends.push(BendConstraint {
                    ids,
                    rest_angle,
                    compliance: bend,
                    lambda: 0.0,
                });
            }
        }
        self.topology_changed();
        Ok(handle)
    }

    /// Builds a rope (see [`ClothRope`]). Nothing is added when the description is rejected.
    ///
    /// # Errors
    /// [`ClothError::Degenerate`] for zero segments or `start == end`;
    /// [`ClothError::NonFinite`] / [`ClothError::Negative`] for bad numbers.
    pub fn add_rope(&mut self, rope: &ClothRope) -> Result<ClothRopeHandle, ClothError> {
        let start = finite_vec(rope.start, "rope start")?;
        let end = finite_vec(rope.end, "rope end")?;
        let inv_mass = inv_mass_of(rope.particle_mass)?;
        let stretch = non_negative(rope.stretch_compliance, "compliance")?;
        let bend = non_negative(rope.bend_compliance, "compliance")?;
        if rope.segments == 0 || rope.segments >= asset::MAX_ASSET_PARTICLES {
            return Err(ClothError::Degenerate {
                what: "rope segments",
            });
        }
        let span = end - start;
        if !(span.length() > EPSILON && span.is_finite()) {
            return Err(ClothError::Degenerate { what: "rope length" });
        }
        let first = self.particles.len();
        let count = rope.segments + 1;
        for i in 0..count {
            let p = start + span * (i as f32 / rope.segments as f32);
            if let Err(err) = self.add_particle(p, inv_mass) {
                self.truncate_particles(first);
                return Err(err);
            }
        }
        for i in first..first + rope.segments {
            self.push_distance(i, i + 1, stretch);
        }
        for i in first..first + count.saturating_sub(2) {
            self.push_distance(i, i + 2, bend);
        }
        self.topology_changed();
        Ok(ClothRopeHandle { first, count })
    }

    /// Distance constraint between two valid, distinct particles (builders only).
    fn push_distance(&mut self, a: usize, b: usize, compliance: f32) {
        let rest = (self.particles[a].position - self.particles[b].position).length();
        self.distance.push(DistanceConstraint {
            a,
            b,
            rest,
            compliance,
            lambda: 0.0,
            active: true,
        });
    }

    fn truncate_particles(&mut self, len: usize) {
        self.particles.truncate(len);
        self.previous.truncate(len);
        self.rest.truncate(len);
        self.contacts.truncate(len);
        self.wind_accel.truncate(len);
        self.layers.truncate(len);
        if self.pose.maps.is_some() {
            self.pose.base_inv_mass.truncate(len);
            self.pose.pull_lambda.truncate(len);
        }
    }

    /// Advances the simulation by `dt` seconds: adds `dt` to the accumulator and runs every
    /// whole fixed step it now holds (at most `max_steps_per_call`; further whole steps are
    /// dropped and counted, the fraction of a step left over is kept). Returns the number of
    /// fixed steps run. A [`ClothState::Paused`] solver runs nothing and keeps no backlog.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] or [`ClothError::Negative`] for `dt`; nothing changes then.
    pub fn step(&mut self, dt: f32) -> Result<u32, ClothError> {
        let dt = non_negative(dt, "dt")?;
        let _span = debug_span!("cloth_step", particles = self.particles.len()).entered();
        let fixed = self.config.time_step;
        self.accumulator += dt;
        if self.state == ClothState::Paused {
            let whole = (self.accumulator / fixed).floor();
            self.paused_steps = self.paused_steps.saturating_add(whole as u32);
            self.accumulator -= whole * fixed;
            if !(0.0..fixed).contains(&self.accumulator) {
                self.accumulator = 0.0;
            }
            return Ok(0);
        }
        let mut ran = 0;
        while self.accumulator >= fixed {
            if ran == self.config.max_steps_per_call {
                let whole = (self.accumulator / fixed).floor();
                // `as` saturates for huge frames, which is what the counter wants.
                self.dropped_steps = self.dropped_steps.saturating_add(whole as u64);
                self.accumulator -= whole * fixed;
                if !(0.0..fixed).contains(&self.accumulator) {
                    self.accumulator = 0.0;
                }
                break;
            }
            self.fixed_step(fixed);
            self.accumulator -= fixed;
            ran += 1;
        }
        Ok(ran)
    }

    /// Runs exactly `count` fixed steps now, also while [`ClothState::Paused`] (agent
    /// `physics.cloth_step`). The accumulator is not touched. `count` is clamped to
    /// [`MAX_STEP_FIXED_COUNT`]; the clamped value is returned.
    pub fn step_fixed(&mut self, count: u32) -> u32 {
        let count = count.min(MAX_STEP_FIXED_COUNT);
        let fixed = self.config.time_step;
        for _ in 0..count {
            self.fixed_step(fixed);
        }
        count
    }

    fn fixed_step(&mut self, step: f32) {
        if self.state == ClothState::Inactive {
            self.snap_to_pose();
            self.consume_inputs();
            self.steps += 1;
            return;
        }
        self.prepare();
        self.check_teleport();
        if self.guard.settings.enabled && self.reaction_cap.is_some() {
            // A faulted step's impulses must not reach the rigid bodies (no allocation once warm).
            self.guard.reaction.clone_from(&self.reaction);
            self.guard.reaction_angular.clone_from(&self.reaction_angular);
        }
        if matches!(
            self.effective_self_collision(),
            SelfCollision::VertexTriangle { .. }
        ) {
            // Capacity stays after the first step: no allocation in a steady state.
            self.step_start.clear();
            self.step_start.extend(self.particles.iter().map(|p| p.position));
        }
        let substeps = self.effective_substeps();
        let h = step / substeps as f32;
        for k in 0..substeps {
            let t0 = k as f32 / substeps as f32;
            let t1 = (k + 1) as f32 / substeps as f32;
            self.substep(h, t0, t1);
            // A step that went non-finite faults anyway (ADR-3273): stop before the next
            // substep's broad phases hash the NaN particles into one cell.
            if self.guard.settings.enabled && !self.particles.iter().all(|p| p.position.is_finite()) {
                break;
            }
        }
        // The guard first: a faulted step is undone wholesale (positions, previous, reaction
        // impulses), and tearing only ever acts on a state that passed — otherwise a blown step
        // would cut links the restore never brings back (ADR-3273, rescue fix).
        self.guard_check();
        self.tear();
        if let Some(model) = self.soft.as_deref_mut() {
            model.after_step(&self.particles);
        }
        self.consume_inputs();
        self.advance_state();
        self.steps += 1;
    }

    /// Rebuilds tethers and self-collision exclusions when the topology changed (allocates;
    /// never runs in a steady state).
    fn prepare(&mut self) {
        if !self.tethers_valid {
            self.build_tethers();
            self.tethers_valid = true;
            // The stiff limit's attachments are rebuilt when it next fires.
            self.guard.tethers_valid = false;
        }
        if self.tuning.self_collision.radius().is_some() {
            let rings = self.tuning.exclude_rings;
            let edge_count = self.distance.len() + 3 * self.triangles.len();
            if !self.selfcol.is_built_for(self.particles.len(), edge_count, rings) {
                let mut edges = Vec::with_capacity(edge_count);
                edges.extend(self.distance.iter().map(|c| (c.a, c.b, c.rest)));
                for t in &self.triangles {
                    for k in 0..3 {
                        let (a, b) = (t[k], t[(k + 1) % 3]);
                        edges.push((a, b, self.rest[a].distance(self.rest[b])));
                    }
                }
                self.selfcol.prepare(self.particles.len(), &edges, rings);
            }
        }
    }

    /// Multi-source shortest paths over the distance links from every pinned particle; each
    /// free particle reached gets a tether to its nearest anchor (ties: lower anchor index).
    fn build_tethers(&mut self) {
        self.tethers.clear();
        if self.tuning.tether.is_none() {
            return;
        }
        let mut tethers = std::mem::take(&mut self.tethers);
        self.shortest_tethers(&mut tethers, false);
        self.tethers = tethers;
    }

    /// The stiff limit's long-range attachments (ADR-3273), over the zero-compliance links only
    /// (compliant links may stretch, so a path through them is no bound). Allocates, so it runs
    /// the first time the limit fires after a topology or pin change, not in a steady state.
    fn build_guard_tethers(&mut self) {
        let mut tethers = std::mem::take(&mut self.guard.tethers);
        self.shortest_tethers(&mut tethers, true);
        self.guard.tethers = tethers;
        self.guard.tethers_valid = true;
    }

    /// The tethers of `build_tethers` into `out` (cleared first); `stiff_only` walks only the
    /// zero-compliance links.
    fn shortest_tethers(&self, out: &mut Vec<Tether>, stiff_only: bool) {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        out.clear();
        let n = self.particles.len();
        let mut adjacency: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
        for c in self
            .distance
            .iter()
            .filter(|c| c.active && (!stiff_only || c.compliance == 0.0))
        {
            adjacency[c.a].push((c.b, c.rest));
            adjacency[c.b].push((c.a, c.rest));
        }
        let mut best: Vec<Option<(f32, usize)>> = vec![None; n];
        let mut heap = BinaryHeap::new();
        for (i, p) in self.particles.iter().enumerate() {
            if p.inv_mass == 0.0 {
                heap.push(Reverse((0u32, i, i)));
            }
        }
        while let Some(Reverse((bits, anchor, node))) = heap.pop() {
            if best[node].is_some() {
                continue;
            }
            let dist = f32::from_bits(bits);
            best[node] = Some((dist, anchor));
            for &(next, len) in &adjacency[node] {
                if best[next].is_none() {
                    // Non-negative floats order like their bit patterns.
                    heap.push(Reverse(((dist + len).to_bits(), anchor, next)));
                }
            }
        }
        for (i, p) in self.particles.iter().enumerate() {
            if p.inv_mass > 0.0
                && let Some((length, anchor)) = best[i]
            {
                out.push(Tether {
                    particle: i,
                    anchor,
                    length,
                });
            }
        }
    }

    fn effective_substeps(&self) -> u32 {
        let authored = self.config.substeps;
        match self.lod {
            0 => authored,
            1 => (authored / 2).max(2).min(authored),
            _ => (authored / 4).max(2).min(authored),
        }
    }

    fn effective_iterations(&self) -> u32 {
        if self.lod == 0 { self.config.iterations } else { 1 }
    }

    fn effective_self_collision(&self) -> SelfCollision {
        match self.lod {
            0 => self.tuning.self_collision,
            1 => self.tuning.self_collision.step_down(),
            _ => SelfCollision::Off,
        }
    }

    /// The wind velocity of this step: the field and global term plus the solver's own term
    /// (gust-modulated), clamped to 60 m/s.
    fn current_wind(&self) -> Vec3 {
        let own = match self.gust {
            Some(g) => self.wind * g.factor(self.steps, self.centroid()),
            None => self.wind,
        };
        tuning::clamp_wind(self.global_wind + self.field_wind + own)
    }

    /// Mean particle position (zero without particles).
    #[must_use]
    pub fn centroid(&self) -> Vec3 {
        if self.particles.is_empty() {
            return Vec3::ZERO;
        }
        let sum: Vec3 = self.particles.iter().map(|p| p.position).sum();
        sum / self.particles.len() as f32
    }

    /// Fills `wind_accel` with each particle's wind acceleration for this substep.
    fn gather_wind(&mut self, wind: Vec3) {
        self.wind_accel.fill(Vec3::ZERO);
        if wind == Vec3::ZERO {
            return;
        }
        let model = if self.lod == 0 {
            self.tuning.wind_model
        } else {
            self.far_push_model(wind)
        };
        match model {
            WindModel::Push { gain } => {
                for (a, p) in self.wind_accel.iter_mut().zip(&self.particles) {
                    if p.inv_mass > 0.0 {
                        *a = wind * gain;
                    }
                }
            }
            WindModel::Aerodynamic { drag, lift } => {
                let rho = self.tuning.air_density;
                for (t, &hidden) in self.triangles.iter().zip(&self.hidden) {
                    if hidden {
                        continue;
                    }
                    let ps = t.map(|i| self.particles[i].position);
                    let vs = t.map(|i| self.particles[i].velocity);
                    let f = wind::triangle_force(ps, vs, wind, rho, drag, lift);
                    for &i in t {
                        self.wind_accel[i] += f * self.particles[i].inv_mass;
                    }
                }
            }
        }
    }

    /// The `Push` model far LODs use for an aerodynamic cloth: the face-on drag acceleration at
    /// the current wind speed, `0.5 ρ drag |w| (area / mass)`.
    fn far_push_model(&self, wind: Vec3) -> WindModel {
        match self.tuning.wind_model {
            push @ WindModel::Push { .. } => push,
            WindModel::Aerodynamic { drag, .. } => {
                let area: f32 = self
                    .triangles
                    .iter()
                    .map(|t| {
                        let p = t.map(|i| self.particles[i].position);
                        0.5 * (p[1] - p[0]).cross(p[2] - p[0]).length()
                    })
                    .sum();
                let mass: f32 = self
                    .particles
                    .iter()
                    .filter(|p| p.inv_mass > 0.0)
                    .map(|p| 1.0 / p.inv_mass)
                    .sum();
                let gain = if mass > 0.0 {
                    0.5 * self.tuning.air_density * drag * wind.length() * area / mass
                } else {
                    0.0
                };
                WindModel::Push { gain }
            }
        }
    }

    fn substep(&mut self, h: f32, t0: f32, t1: f32) {
        let gravity = self.tuning.gravity(self.config.gravity);
        let damping = if self.state == ClothState::Settling {
            2.0 * self.config.damping
        } else {
            self.config.damping
        };
        let damp = (1.0 - damping * h).max(0.0);
        let bound = self.pose.maps.is_some() && self.pose.has_targets();

        // 0. Pose targets at the substep end; root inertia.
        if bound {
            self.pose.interpolate(t1);
        }
        if self.tuning.inertia_scale < 1.0
            && let (Some(r0), Some(r1)) = (self.pose.root_at(t0), self.pose.root_at(t1))
        {
            let keep = 1.0 - self.tuning.inertia_scale;
            let rotate = r1.rotation * r0.rotation.inverse();
            for p in &mut self.particles {
                if p.inv_mass > 0.0 {
                    let moved = rotate * (p.position - r0.translation) + r1.translation;
                    p.position += (moved - p.position) * keep;
                    p.velocity = p.velocity + (rotate * p.velocity - p.velocity) * keep;
                }
            }
        }

        // 1. Wind and prediction.
        let wind = self.current_wind();
        self.last_wind = wind;
        {
            let _span = debug_span!("cloth_wind").entered();
            self.gather_wind(wind);
        }
        for (((p, prev), contact), accel) in self
            .particles
            .iter_mut()
            .zip(self.previous.iter_mut())
            .zip(self.contacts.iter_mut())
            .zip(&self.wind_accel)
        {
            *prev = p.position;
            *contact = Contact::default();
            if p.inv_mass > 0.0 {
                p.velocity = (p.velocity + (gravity + *accel) * h) * damp;
                p.position += p.velocity * h;
            }
        }
        // Zero-reach particles follow their target exactly.
        if bound && let Some(maps) = &self.pose.maps {
            for (i, p) in self.particles.iter_mut().enumerate() {
                if maps.reach_of(i) == 0.0
                    && let Some(target) = self.pose.sub.get(i)
                {
                    p.position = target.position;
                }
            }
        }

        if let Some(model) = self.soft.as_deref_mut() {
            model.after_predict(&mut self.particles, h);
            model.reset_lambdas();
        }

        // 2. Fresh multipliers each substep (XPBD Algorithm 1).
        for c in &mut self.distance {
            c.lambda = 0.0;
        }
        for c in &mut self.bends {
            c.lambda = 0.0;
        }
        self.pose.pull_lambda.fill(0.0);

        // Self-collision broad phase, collider shapes for this substep.
        let self_mode = self.effective_self_collision();
        let self_radius = self_mode.radius();
        let vertex_triangle = matches!(self_mode, SelfCollision::VertexTriangle { .. });
        let layer_gap = self_radius.map_or(0.0, |r| self.layer_gap.unwrap_or(2.0 * r));
        if let Some(r) = self_radius {
            let _span = debug_span!("cloth_self_collide").entered();
            if vertex_triangle {
                let input = vtcol::VtInput {
                    particles: &self.particles,
                    previous: &self.previous,
                    step_start: &self.step_start,
                    rest: &self.rest,
                    triangles: &self.triangles,
                    hidden: &self.hidden,
                    layers: &self.layers,
                    radius: r,
                    layer_gap,
                };
                self.vtcol.broad_phase(&input, &self.selfcol);
            } else {
                self.selfcol
                    .broad_phase(&self.particles, &self.rest, r, &self.layers, layer_gap);
            }
        }
        self.collider_from.clear();
        self.collider_to.clear();
        for (cur, prev) in self.colliders.iter().zip(&self.collider_prev) {
            match prev {
                Some(prev) => {
                    self.collider_from.push(lerp_collider(prev, cur, t0));
                    self.collider_to.push(lerp_collider(prev, cur, t1));
                }
                None => {
                    self.collider_from.push(*cur);
                    self.collider_to.push(*cur);
                }
            }
        }

        // 3. Project.
        let inv_h2 = 1.0 / (h * h);
        let tether_scale = self.tuning.tether.unwrap_or(f32::INFINITY);
        // The authored iterations, then more while a sweep finds a zero-compliance link over the
        // stiff limit (ADR-3273); without a limit the check never fires.
        let authored = self.effective_iterations();
        let (stiff_limit, most) = match self.guard.settings.stiff_limit {
            Some(limit) => (limit, authored + self.guard.settings.extra_iterations),
            None => (f32::INFINITY, authored),
        };
        let mut done = 0;
        let mut over = false;
        while done < authored || (over && done < most) {
            done += 1;
            over = solve_distances(&mut self.particles, &mut self.distance, inv_h2, stiff_limit);
            solve_bends(&mut self.particles, &mut self.bends, inv_h2);
            if let Some(model) = self.soft.as_deref_mut() {
                model.project(&mut self.particles, inv_h2);
            }
            solve_tethers(&mut self.particles, &self.tethers, tether_scale);
            if bound {
                let _span = debug_span!("cloth_pose_constraints").entered();
                skin::solve_pose(
                    &mut self.particles,
                    &mut self.pose,
                    self.tuning.pull_compliance,
                    self.tuning.pull_scale,
                    inv_h2,
                );
            }
            if let Some(r) = self_radius {
                if vertex_triangle {
                    self.vtcol.project(
                        &mut self.particles,
                        &self.previous,
                        &self.triangles,
                        self.tuning.self_friction,
                    );
                } else {
                    self.selfcol.project(
                        &mut self.particles,
                        &self.previous,
                        r,
                        self.tuning.self_friction,
                        &self.layers,
                        layer_gap,
                    );
                }
            }
            let _span = debug_span!("cloth_collide").entered();
            self.collide(snapshot::SubstepFrame {
                t0,
                t1,
                h,
                thickness: self.config.thickness,
            });
        }
        if vertex_triangle {
            self.vtcol.measure(&self.particles, &self.triangles);
        } else {
            self.vtcol.crossings = 0;
        }
        if let Some(cap) = self.tuning.strain_cap {
            solve_strain_caps(&mut self.particles, &self.distance, cap);
        }
        self.guard.extra_iterations += u64::from(done - authored);
        if over {
            // Still over after the extra iterations: long-range attachments bound the sheet's
            // overall stretch in one pass (Kim et al. 2012), then local sweeps the links.
            if !self.guard.tethers_valid {
                self.build_guard_tethers();
            }
            // Corrections become velocity like any projection's; only a move faster than
            // `ATTACHMENT_SPEED_MAX` (a sheet hauled after a teleported pin) keeps its excess out.
            let target = self.guard.settings.stiff_target;
            solve_guard_tethers(
                &mut self.particles,
                &mut self.previous,
                &self.guard.tethers,
                target,
                ATTACHMENT_SPEED_MAX * h,
            );
            solve_stiff_limit(
                &mut self.particles,
                &mut self.previous,
                &self.distance,
                target,
                ATTACHMENT_SPEED_MAX * h,
            );
            // Colliders last, as in every iteration: a substep never ends inside a shape.
            self.collide(snapshot::SubstepFrame {
                t0,
                t1,
                h,
                thickness: self.config.thickness,
            });
            self.guard.stiff_projections += 1;
        }

        // 4. Velocities and friction.
        let mut touching = 0;
        for ((p, prev), contact) in self.particles.iter_mut().zip(&self.previous).zip(&self.contacts) {
            if p.inv_mass == 0.0 {
                continue;
            }
            let mut velocity = (p.position - *prev) / h;
            if contact.depth > 0.0 {
                touching += 1;
                // `p.velocity` still holds the velocity the substep predicted with.
                velocity = contact_velocity(velocity, p.velocity, contact, h);
            }
            // Flush denormal speeds to zero (spec 55 § 9).
            if velocity.length_squared() < 1e-24 {
                velocity = Vec3::ZERO;
            }
            p.velocity = velocity;
        }
        if vertex_triangle {
            self.vtcol.inelastic(&mut self.particles, &self.triangles);
        }
        self.last_contacts = touching;
    }

    /// Pushes free particles out of the colliders and the world snapshot (the last pass of every
    /// iteration).
    fn collide(&mut self, frame: snapshot::SubstepFrame) {
        solve_collisions(
            &mut self.particles,
            &mut self.contacts,
            &self.previous,
            &self.collider_from,
            &self.collider_to,
            frame,
        );
        if !self.snapshot.is_empty() {
            let reaction = if self.reaction_cap.is_some() {
                Some((&mut self.reaction[..], &mut self.reaction_angular[..]))
            } else {
                None
            };
            solve_snapshot(
                &mut self.particles,
                &mut self.contacts,
                &self.previous,
                &self.snapshot,
                frame,
                reaction,
            );
        }
    }

    /// Cuts over-strained links at the end of a fixed step (spec 55 § 2.5).
    fn tear(&mut self) {
        let Some(tear) = self.tuning.tear else {
            return;
        };
        let mut cut = 0;
        for (index, c) in self.distance.iter_mut().enumerate() {
            if cut >= tear.max_per_step {
                break;
            }
            if !c.active || c.rest <= EPSILON {
                continue;
            }
            let (pa, pb) = (self.particles[c.a], self.particles[c.b]);
            if pa.inv_mass == 0.0 || pb.inv_mass == 0.0 {
                continue;
            }
            if (pa.position - pb.position).length() / c.rest > tear.strain {
                c.active = false;
                cut += 1;
                self.torn += 1;
                self.torn_events.push(ClothTorn {
                    constraint: index,
                    step: self.steps,
                });
                for (t, hidden) in self.triangles.iter().zip(self.hidden.iter_mut()) {
                    if t.contains(&c.a) && t.contains(&c.b) {
                        *hidden = true;
                    }
                }
            }
        }
        if cut > 0 {
            self.tethers_valid = false;
        }
    }

    /// Largest `length / rest length` over the active zero-compliance links with a non-zero rest
    /// length (0 when there are none). NaN lengths are skipped; an overflowing one gives
    /// infinity. Compliant links are never counted: an elastic cloth may stretch past any
    /// ratio, and the guard must not turn `max_strain` into a stiffness it cannot override
    /// (ADR-3273, rescue fix).
    fn strain_now(&self) -> f32 {
        let mut worst = 0.0f32;
        for c in &self.distance {
            if !c.active || c.rest <= EPSILON || c.compliance != 0.0 {
                continue;
            }
            let d2 = (self.particles[c.a].position - self.particles[c.b].position).length_squared();
            let ratio2 = d2 / (c.rest * c.rest);
            // A squared length can overflow to +inf on a runaway link; the readout is the
            // largest *finite* ratio (0 when none was finite), per `ClothFault::strain`.
            if ratio2.is_finite() && ratio2 > worst {
                worst = ratio2;
            }
        }
        worst.sqrt()
    }

    /// Whether an active zero-compliance link is longer than `max_strain` times its rest length
    /// without the moved-pin excuse: its own baseline — the last good step's free particles
    /// against this link's pins' current places — is over `max_strain` too. A pin game code
    /// parked far away stretches its own links past the limit before the step, and the solver
    /// pulling the cloth after it must not fault on them (the baseline stays over the limit
    /// while the pin stays put). A link whose baseline is within the limit is held to it no
    /// matter how far apart an unrelated pin pair sits (ADR-3273, rescue fix): the baseline of
    /// a link no pin move stretched can never cross `max_strain` without a fault first, so the
    /// excuse cannot ratchet. Without a matching last good copy nothing is excused.
    fn unexcused_stiff_link_over(&self, max_strain: f32) -> bool {
        if self.guard.good.len() != self.particles.len() {
            return true;
        }
        let at = |i: usize| {
            let p = &self.particles[i];
            if p.inv_mass == 0.0 {
                p.position
            } else {
                self.guard.good[i].position
            }
        };
        let limit2 = max_strain * max_strain;
        for c in &self.distance {
            if !c.active || c.rest <= EPSILON || c.compliance != 0.0 {
                continue;
            }
            let ratio2 = (self.particles[c.a].position - self.particles[c.b].position).length_squared()
                / (c.rest * c.rest);
            if ratio2 > limit2 {
                let before2 = (at(c.a) - at(c.b)).length_squared() / (c.rest * c.rest);
                // A NaN baseline means a non-finite pin (the NonFinite check faults first).
                if before2.is_nan() {
                    continue;
                }
                if before2 <= limit2 {
                    return true;
                }
            }
        }
        false
    }

    /// Largest zero-compliance link ratio of the last good step's free particles with the pins
    /// where they are now (0 without a matching last good copy; NaN gives infinity). Decides
    /// whether the pins keep their places on a fault: they do unless their current places
    /// already stretch a stiff link past `max_strain` against the last good free particles.
    fn moved_pin_strain(&self) -> f32 {
        if self.guard.good.len() != self.particles.len() {
            return 0.0;
        }
        let at = |i: usize| {
            let p = &self.particles[i];
            if p.inv_mass == 0.0 {
                p.position
            } else {
                self.guard.good[i].position
            }
        };
        let mut before = 0.0f32;
        for c in &self.distance {
            if !c.active || c.rest <= EPSILON || c.compliance != 0.0 {
                continue;
            }
            let ratio2 = (at(c.a) - at(c.b)).length_squared() / (c.rest * c.rest);
            if ratio2.is_nan() {
                return f32::INFINITY;
            }
            if ratio2 > before {
                before = ratio2;
            }
        }
        before.sqrt()
    }

    /// The runaway guard at the end of a fixed step (ADR-3273, see [`ClothGuard`]). A step that
    /// passes only refreshes the one-deep copy (no allocation once its capacity is reached).
    fn guard_check(&mut self) {
        if !self.guard.settings.enabled {
            return;
        }
        let _span = debug_span!("cloth_guard").entered();
        let bad = self
            .particles
            .iter()
            .position(|p| !(p.position.is_finite() && p.velocity.is_finite()));
        let strain = self.strain_now();
        let max_strain = self.guard.settings.max_strain;
        let kind = if bad.is_some() {
            ClothFaultKind::NonFinite
        } else if strain > max_strain && self.unexcused_stiff_link_over(max_strain) {
            ClothFaultKind::Runaway
        } else {
            self.guard.streak = 0;
            self.guard.max_strain = strain;
            self.guard.peak_strain = self.guard.peak_strain.max(strain);
            self.guard.good.clone_from(&self.particles);
            return;
        };
        if self.reaction_cap.is_some() && self.guard.reaction.len() == self.reaction.len() {
            self.reaction.clone_from(&self.guard.reaction);
            self.reaction_angular.clone_from(&self.guard.reaction_angular);
        }
        // Pins keep where the caller put them since the last good step, unless that place
        // stretches a stiff link past `max_strain` against the last good free particles (a pin
        // thrown far away is the likely cause of the fault, so it goes back too).
        let keep_pins = self.moved_pin_strain() <= max_strain;
        let recovery = if self.guard.streak == 0 && self.guard.good.len() == self.particles.len() {
            for (p, good) in self.particles.iter_mut().zip(&self.guard.good) {
                if !(keep_pins && p.inv_mass == 0.0) {
                    p.position = good.position;
                }
                p.velocity = Vec3::ZERO;
            }
            for (prev, p) in self.previous.iter_mut().zip(&self.particles) {
                *prev = p.position;
            }
            ClothRecovery::LastGood
        } else {
            // The step loop owns the accumulator; a reset inside a fixed step must not move it.
            let accumulator = self.accumulator;
            self.reset_to_pose();
            self.accumulator = accumulator;
            // Pinned particles keep their place in a reset; one the fault reached goes back to
            // where it was added, and so do all of them when their places still stretch a link
            // past `max_strain` (a pin thrown far away would fault every step).
            for (p, rest) in self.particles.iter_mut().zip(&self.rest) {
                if !(p.position.is_finite() && p.velocity.is_finite()) {
                    p.position = *rest;
                    p.velocity = Vec3::ZERO;
                }
            }
            if self.strain_now() > max_strain {
                for (p, rest) in self.particles.iter_mut().zip(&self.rest) {
                    p.position = *rest;
                    p.velocity = Vec3::ZERO;
                }
            }
            for (prev, p) in self.previous.iter_mut().zip(&self.particles) {
                *prev = p.position;
            }
            self.guard.pose_resets += 1;
            ClothRecovery::RestPose
        };
        self.guard.streak = self.guard.streak.saturating_add(1);
        let fault = ClothFault {
            step: self.steps,
            kind,
            recovery,
            particle: bad,
            strain,
        };
        self.guard.faults += 1;
        match kind {
            ClothFaultKind::NonFinite => self.guard.non_finite += 1,
            ClothFaultKind::Runaway => self.guard.runaway += 1,
        }
        self.guard.last = Some(fault);
        if self.guard.events.len() < FAULT_EVENTS_MAX {
            self.guard.events.push(fault);
        }
        self.guard.max_strain = self.strain_now();
        warn!(
            target: "regia_physics",
            step = fault.step,
            code = fault.code(),
            recovery = recovery.name(),
            strain,
            "cloth guard fault"
        );
    }

    /// The previous pose and collider shapes were used by this step.
    fn consume_inputs(&mut self) {
        if self.pose.has_targets() {
            let cur = std::mem::take(&mut self.pose.cur);
            self.pose.prev.clone_from(&cur);
            self.pose.cur = cur;
        }
        self.pose.root_prev = self.pose.root_cur;
        self.collider_prev.fill(None);
        self.snapshot.advance();
    }

    fn advance_state(&mut self) {
        self.state_steps = self.state_steps.saturating_add(1);
        match self.state {
            ClothState::Settling => {
                self.ramp = (self.ramp + RAMP_FULL / lod::BLEND_IN_STEPS).min(RAMP_FULL);
                if self.state_steps >= lod::SETTLE_STEPS {
                    self.set_state(ClothState::Running);
                }
            }
            ClothState::Running => {
                self.ramp = (self.ramp + RAMP_FULL / lod::BLEND_IN_STEPS).min(RAMP_FULL);
            }
            ClothState::Fading => {
                self.ramp = self.ramp.saturating_sub(RAMP_FULL / lod::BLEND_OUT_STEPS);
                if self.ramp == 0 {
                    self.set_state(ClothState::Inactive);
                }
            }
            ClothState::Inactive | ClothState::Paused => {}
        }
    }

    fn set_state(&mut self, state: ClothState) {
        if self.state != state {
            self.state = state;
            self.state_steps = 0;
        }
    }

    /// Snaps the cloth to its pose (or, without a pose, to the positions the particles were
    /// added at), zeroes every velocity and enters [`ClothState::Settling`] with the blend
    /// ramp at 0.
    pub fn reset_to_pose(&mut self) {
        self.snap_to_pose();
        if !self.pose.has_targets() {
            for (p, rest) in self.particles.iter_mut().zip(&self.rest) {
                if p.inv_mass > 0.0 {
                    p.position = *rest;
                }
            }
        }
        self.settle_in_place();
    }

    /// Wakes from a long pause or from LOD off: back to the pose. A soft body with no pose targets
    /// has no pose to go back to (its rest is where it was built), so it settles where it is.
    fn wake(&mut self) {
        if self.soft.is_some() && !self.pose.has_targets() {
            self.settle_in_place();
        } else {
            self.reset_to_pose();
        }
    }

    /// Zero velocity at the current positions, then [`ClothState::Settling`].
    fn settle_in_place(&mut self) {
        for p in &mut self.particles {
            p.velocity = Vec3::ZERO;
        }
        for (prev, p) in self.previous.iter_mut().zip(&self.particles) {
            *prev = p.position;
        }
        self.selfcol.pairs.clear();
        // The last good copy is from before the reset: a fault now goes to the pose.
        self.guard.good.clear();
        self.accumulator = 0.0;
        self.ramp = 0;
        self.paused_steps = 0;
        self.state = ClothState::Settling;
        self.state_steps = 0;
    }

    fn snap_to_pose(&mut self) {
        if !self.pose.has_targets() {
            return;
        }
        for (p, t) in self.particles.iter_mut().zip(&self.pose.cur) {
            p.position = t.position;
            p.velocity = Vec3::ZERO;
        }
    }

    fn check_teleport(&mut self) {
        let jump = match (self.pose.root_prev, self.pose.root_cur) {
            (Some(a), Some(b)) => a.translation.distance(b.translation),
            _ if self.pose.has_targets() => self
                .pose
                .prev
                .iter()
                .zip(&self.pose.cur)
                .map(|(a, b)| a.position.distance(b.position))
                .fold(0.0, f32::max),
            _ => 0.0,
        };
        if jump > self.tuning.teleport_distance {
            self.teleports += 1;
            // The jump itself is not simulated: the previous targets become the current ones.
            if self.pose.has_targets() {
                self.pose.prev.clone_from(&self.pose.cur);
            }
            self.pose.root_prev = self.pose.root_cur;
            self.reset_to_pose();
        }
    }

    // -----------------------------------------------------------------------
    // Spec 55 setters and queries
    // -----------------------------------------------------------------------

    /// The spec 55 settings in use.
    #[must_use]
    pub fn tuning(&self) -> &ClothTuning {
        &self.tuning
    }

    /// Replaces the spec 55 settings.
    ///
    /// # Errors
    /// [`ClothTuning::validate`]'s error; nothing changes then.
    pub fn set_tuning(&mut self, tuning: ClothTuning) -> Result<(), ClothError> {
        tuning.validate()?;
        if tuning.tether != self.tuning.tether {
            self.tethers_valid = false;
        }
        self.tuning = tuning;
        Ok(())
    }

    /// Replaces the baseline settings (substeps, iterations, damping, thickness, ...).
    ///
    /// # Errors
    /// [`ClothConfig::validate`]'s error; nothing changes then.
    pub fn set_config(&mut self, config: ClothConfig) -> Result<(), ClothError> {
        config.validate()?;
        self.config = config;
        Ok(())
    }

    /// Binds particles to their pose with `maps` (spec 55 § 2.2). Particles with `reach == 0`
    /// get inverse mass 0 and follow their target; `mass_scale` divides the inverse mass. The
    /// solver enters [`ClothState::Settling`] (snapping to the targets when it has them).
    ///
    /// # Errors
    /// [`PoseMaps::validate`]'s error; nothing changes then.
    pub fn set_pose_maps(&mut self, maps: PoseMaps) -> Result<(), ClothError> {
        maps.validate(self.particles.len())?;
        if self.pose.maps.is_none() {
            self.pose.base_inv_mass = self.particles.iter().map(|p| p.inv_mass).collect();
            self.pose.pull_lambda = vec![0.0; self.particles.len()];
        }
        self.pose.maps = Some(maps);
        for (i, p) in self.particles.iter_mut().enumerate() {
            p.inv_mass = self.pose.effective_inv_mass(i, self.pose.base_inv_mass[i]);
            if p.inv_mass == 0.0 {
                p.velocity = Vec3::ZERO;
            }
        }
        self.tethers_valid = false;
        self.reset_to_pose();
        Ok(())
    }

    /// Removes the pose binding: inverse masses go back to their unscaled values, targets and
    /// root motion are dropped, and the solver runs free ([`ClothState::Running`], blend 1).
    pub fn clear_pose(&mut self) {
        if self.pose.maps.is_some() {
            for (p, &base) in self.particles.iter_mut().zip(&self.pose.base_inv_mass) {
                p.inv_mass = base;
            }
        }
        self.pose = skin::PoseBinding::default();
        self.tethers_valid = false;
        self.state = ClothState::Running;
        self.state_steps = 0;
        self.ramp = RAMP_FULL;
    }

    /// The pose maps, when the solver is bound.
    #[must_use]
    pub fn pose_maps(&self) -> Option<&PoseMaps> {
        self.pose.maps.as_ref()
    }

    /// Sets the pose targets of the previous and the current fixed step (one per particle).
    /// The next fixed step interpolates between them per substep; later fixed steps in the
    /// same call hold `cur`. The first targets snap the cloth to its pose. A jump longer than
    /// `teleport_distance` (of the root, when [`ClothSolver::set_root_motion`] was called,
    /// else of any target) resets the cloth to the pose.
    ///
    /// # Errors
    /// [`ClothError::PoseSizeMismatch`] or [`ClothError::NonFinite`]; nothing changes then.
    pub fn set_pose_targets(&mut self, prev: &[PoseTarget], cur: &[PoseTarget]) -> Result<(), ClothError> {
        let n = self.particles.len();
        // Both lists are checked before either buffer is touched (a later error changes nothing).
        skin::validate_targets(prev, n)?;
        skin::validate_targets(cur, n)?;
        let first = !self.pose.has_targets();
        skin::write_targets(&mut self.pose.prev, prev);
        skin::write_targets(&mut self.pose.cur, cur);
        if first && self.pose.maps.is_some() {
            self.reset_to_pose();
        }
        Ok(())
    }

    /// Sets the root joint's pose at the previous and the current fixed step: drives root
    /// inertia (`ClothTuning::inertia_scale`) and teleport detection.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`]; nothing changes then.
    pub fn set_root_motion(&mut self, prev: Isometry, cur: Isometry) -> Result<(), ClothError> {
        for iso in [prev, cur] {
            finite_vec(iso.translation, "root")?;
            if !iso.rotation.is_finite() || iso.rotation.length_squared() <= EPSILON {
                return Err(ClothError::NonFinite { what: "root" });
            }
        }
        self.pose.root_prev = Some(Isometry::new(prev.rotation.normalize(), prev.translation));
        self.pose.root_cur = Some(Isometry::new(cur.rotation.normalize(), cur.translation));
        Ok(())
    }

    /// Sets the game's blend between pose (0) and simulation (1). The output uses it times the
    /// state ramp.
    ///
    /// # Errors
    /// [`ClothError::BadBlend`] outside `[0, 1]` or NaN.
    pub fn set_blend(&mut self, blend: f32) -> Result<(), ClothError> {
        if !(0.0..=1.0).contains(&blend) {
            return Err(ClothError::BadBlend);
        }
        self.blend = blend;
        Ok(())
    }

    /// The blend the output uses (the game's blend times the state ramp; 0 when inactive).
    #[must_use]
    pub fn blend(&self) -> f32 {
        if self.state == ClothState::Inactive {
            0.0
        } else {
            self.blend * (self.ramp as f32 / RAMP_FULL as f32)
        }
    }

    /// Records collider `index`'s shape at the previous fixed step. The next fixed step moves
    /// the collider from it to the current shape across the substeps, and contacts use the
    /// collider's velocity for friction and push direction. Consumed by that step.
    ///
    /// # Errors
    /// [`ClothError::ColliderOutOfRange`] and the shape errors of
    /// [`ClothSolver::add_collider`].
    pub fn set_collider_previous(&mut self, index: usize, previous: ClothShape) -> Result<(), ClothError> {
        let len = self.colliders.len();
        let Some(current) = self.colliders.get(index) else {
            return Err(ClothError::ColliderOutOfRange { index, len });
        };
        let collider = ClothCollider {
            shape: previous,
            friction: current.friction,
        }
        .normalized()?;
        self.collider_prev[index] = Some(collider);
        Ok(())
    }

    /// Replaces the world snapshot (spec 55 § 2.4): rigid shapes near the cloth, moving from
    /// their `prev` to their `cur` pose across the next fixed step. The snapshot stays until
    /// replaced; after each step its shapes hold their `cur` pose.
    pub fn set_snapshot(&mut self, snap: WorldSnapshot) {
        self.reaction.clear();
        self.reaction.resize(snap.shapes().len(), Vec3::ZERO);
        self.reaction_angular.clear();
        self.reaction_angular.resize(snap.shapes().len(), Vec3::ZERO);
        self.snapshot = snap;
    }

    /// Keeps the current snapshot and zeroes its reaction.
    ///
    /// The glue uses this when the rigid shapes near the cloth have not moved and have not been
    /// edited, so the snapshot the previous step advanced (`prev` already equal to `cur`) is
    /// still the one a fresh build would produce.
    pub fn reuse_snapshot(&mut self) {
        self.reaction.fill(Vec3::ZERO);
        self.reaction_angular.fill(Vec3::ZERO);
    }

    /// The world snapshot in use.
    #[must_use]
    pub fn snapshot(&self) -> &WorldSnapshot {
        &self.snapshot
    }

    /// Switches two-way reaction on (`Some(max_impulse)`, the per-body cap in N·s the glue
    /// applies per fixed step, spec default 5) or off (`None`, the default). While on, every
    /// world-snapshot push adds `-m * push / h` to its shape's impulse, read back with
    /// [`ClothSolver::take_reaction`].
    ///
    /// # Errors
    /// [`ClothError::BadParam`] for a cap that is not positive and finite.
    pub fn set_reaction(&mut self, max_impulse: Option<f32>) -> Result<(), ClothError> {
        if let Some(cap) = max_impulse
            && !(cap.is_finite() && cap > 0.0)
        {
            return Err(ClothError::BadParam {
                what: "max_reaction_impulse",
            });
        }
        self.reaction_cap = max_impulse;
        self.reaction.iter_mut().for_each(|r| *r = Vec3::ZERO);
        self.reaction_angular.iter_mut().for_each(|r| *r = Vec3::ZERO);
        Ok(())
    }

    /// The per-body reaction cap, when two-way reaction is on.
    #[must_use]
    pub fn reaction(&self) -> Option<f32> {
        self.reaction_cap
    }

    /// Appends `(snapshot key, impulse)` for every snapshot shape that received an impulse
    /// since the last call (in key order) and clears them (the angular part too; see
    /// [`ClothSolver::take_reaction_wrench`]). Empty while reaction is off.
    pub fn take_reaction(&mut self, out: &mut Vec<(SpawnKey, Vec3)>) {
        let shapes = self.snapshot.shapes().iter();
        for ((shape, impulse), angular) in shapes
            .zip(self.reaction.iter_mut())
            .zip(self.reaction_angular.iter_mut())
        {
            if *impulse != Vec3::ZERO {
                out.push((shape.key, *impulse));
            }
            *impulse = Vec3::ZERO;
            *angular = Vec3::ZERO;
        }
    }

    /// As [`ClothSolver::take_reaction`] with the angular part: for every snapshot shape that
    /// received an impulse since the last call (in key order), the linear impulse and the
    /// angular impulse about the shape's current origin ([`ClothReaction::about`]), then clears
    /// them. A rigid body whose center of mass is at `com` receives the torque impulse
    /// `angular_impulse + (about - com) × impulse` (spec 55 § 2.4, ADR-2425).
    pub fn take_reaction_wrench(&mut self, out: &mut Vec<ClothReaction>) {
        let shapes = self.snapshot.shapes().iter();
        for ((shape, impulse), angular) in shapes
            .zip(self.reaction.iter_mut())
            .zip(self.reaction_angular.iter_mut())
        {
            if *impulse != Vec3::ZERO || *angular != Vec3::ZERO {
                out.push(ClothReaction {
                    key: shape.key,
                    impulse: *impulse,
                    angular_impulse: *angular,
                    about: shape.cur.translation,
                });
            }
            *impulse = Vec3::ZERO;
            *angular = Vec3::ZERO;
        }
    }

    /// Puts particles on garment layers (spec 55 § 2.6): `layers[i]` is particle `i`'s layer,
    /// higher is farther out. A self-collision pair across two layers keeps an extra
    /// [`ClothSolver::layer_gap`] and moves only its outer side, so inner layers push outer
    /// layers and never the reverse. An empty slice clears the layers (the default: everything on
    /// layer 0, which behaves exactly like a solver without layers). Particles added later are on
    /// layer 0.
    ///
    /// # Errors
    /// [`ClothError::PoseSizeMismatch`] when `layers` is neither empty nor one per particle.
    pub fn set_layers(&mut self, layers: &[u8]) -> Result<(), ClothError> {
        if !layers.is_empty() && layers.len() != self.particles.len() {
            return Err(ClothError::PoseSizeMismatch {
                len: layers.len(),
                expected: self.particles.len(),
            });
        }
        self.layers.clear();
        if layers.iter().any(|&l| l != 0) {
            self.layers.extend_from_slice(layers);
        }
        Ok(())
    }

    /// Puts the particles `first..first + count` on `layer` (a garment layer per vertex set,
    /// see [`ClothSolver::set_layers`]).
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`] when the range passes the last particle.
    pub fn set_layer_range(&mut self, first: usize, count: usize, layer: u8) -> Result<(), ClothError> {
        let len = self.particles.len();
        let end = first
            .checked_add(count)
            .filter(|&e| e <= len)
            .ok_or(ClothError::ParticleOutOfRange {
                index: first.saturating_add(count).saturating_sub(1),
                len,
            })?;
        if self.layers.is_empty() && layer == 0 {
            return Ok(());
        }
        self.layers.resize(len, 0);
        self.layers[first..end].fill(layer);
        if self.layers.iter().all(|&l| l == 0) {
            self.layers.clear();
        }
        Ok(())
    }

    /// Vertex-triangle pairs whose particle ended the last substep on the wrong side of its
    /// triangle (0 when nothing crossed; always 0 outside `VertexTriangle` mode).
    #[must_use]
    pub fn self_crossings(&self) -> usize {
        self.vtcol.crossings
    }

    /// Highest garment layer in use (0 without layers).
    #[must_use]
    pub fn max_layer(&self) -> u8 {
        self.layers.iter().copied().max().unwrap_or(0)
    }

    /// Garment layer of every particle (empty when every particle is on layer 0).
    #[must_use]
    pub fn layers(&self) -> &[u8] {
        &self.layers
    }

    /// Sets the extra contact distance across layers in meters (`None`: twice the
    /// self-collision radius, the spec default).
    ///
    /// # Errors
    /// [`ClothError::NonFinite`], [`ClothError::Negative`].
    pub fn set_layer_gap(&mut self, gap: Option<f32>) -> Result<(), ClothError> {
        if let Some(g) = gap {
            non_negative(g, "layer gap")?;
        }
        self.layer_gap = gap;
        Ok(())
    }

    /// The extra contact distance across layers when set (`None`: twice the radius).
    #[must_use]
    pub fn layer_gap(&self) -> Option<f32> {
        self.layer_gap
    }

    /// Takes over the motion and lifecycle of `old`, the solver of another sim mesh of the same
    /// cloth (a sim-mesh LOD switch, spec 55 § 2.9). `map` holds, per particle of this solver,
    /// the `old` particle it continues and its rest-pose offset from that particle (so that
    /// several particles of a finer mesh mapped to one coarse particle do not collapse onto it).
    /// Positions, velocities and pose targets move over; so do the state, the blend and its
    /// ramp, the LOD level, the wind, the reaction setting and the step counters, so the switch
    /// neither settles nor counts as a teleport. Cut links stay behind (the new mesh is whole).
    ///
    /// # Errors
    /// [`ClothError::PoseSizeMismatch`] when `map` does not cover this solver,
    /// [`ClothError::ParticleOutOfRange`] for an index past `old`; nothing changes then.
    pub fn adopt_state(&mut self, old: &ClothSolver, map: &[(u32, Vec3)]) -> Result<(), ClothError> {
        if map.len() != self.particles.len() {
            return Err(ClothError::PoseSizeMismatch {
                len: map.len(),
                expected: self.particles.len(),
            });
        }
        if let Some((index, _)) = map.iter().find(|(i, _)| *i as usize >= old.particles.len()) {
            return Err(ClothError::ParticleOutOfRange {
                index: *index as usize,
                len: old.particles.len(),
            });
        }
        for (p, (from, offset)) in self.particles.iter_mut().zip(map) {
            let source = &old.particles[*from as usize];
            p.position = source.position + *offset;
            p.velocity = source.velocity;
        }
        for (prev, p) in self.previous.iter_mut().zip(&self.particles) {
            *prev = p.position;
        }
        if old.pose.has_targets() {
            self.pose.cur = map.iter().map(|(from, _)| old.pose.cur[*from as usize]).collect();
            self.pose.prev = map
                .iter()
                .map(|(from, _)| old.pose.prev[*from as usize])
                .collect();
        }
        self.pose.root_prev = old.pose.root_prev;
        self.pose.root_cur = old.pose.root_cur;
        self.wind = old.wind;
        self.gust = old.gust;
        self.field_wind = old.field_wind;
        self.global_wind = old.global_wind;
        self.reaction_cap = old.reaction_cap;
        self.state = old.state;
        self.state_steps = old.state_steps;
        self.ramp = old.ramp;
        self.blend = old.blend;
        self.lod = old.lod;
        self.paused_steps = old.paused_steps;
        self.teleports = old.teleports;
        self.accumulator = old.accumulator;
        self.steps = old.steps;
        self.dropped_steps = old.dropped_steps;
        Ok(())
    }

    /// Frees a pinned particle with inverse mass `inv_mass` (positive).
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::NonFinite`], [`ClothError::BadParam`]
    /// for a zero or negative inverse mass.
    pub fn unpin(&mut self, index: usize, inv_mass: f32) -> Result<(), ClothError> {
        self.check_particle(index)?;
        if finite(inv_mass, "inverse mass")? <= 0.0 {
            return Err(ClothError::BadParam { what: "inv_mass" });
        }
        self.set_inv_mass(index, inv_mass)
    }

    /// Adds a triangle (for wind, normals and tearing) and returns its index. Grids add theirs.
    ///
    /// # Errors
    /// [`ClothError::ParticleOutOfRange`], [`ClothError::RepeatedParticle`].
    pub fn add_triangle(&mut self, a: usize, b: usize, c: usize) -> Result<usize, ClothError> {
        self.check_distinct(&[a, b, c])?;
        self.triangles.push([a, b, c]);
        self.hidden.push(false);
        self.topology_changed();
        Ok(self.triangles.len() - 1)
    }

    /// Every triangle, in insertion order.
    #[must_use]
    pub fn triangles(&self) -> &[[usize; 3]] {
        &self.triangles
    }

    /// Whether triangle `index` is shown (false once tearing cut one of its edges).
    #[must_use]
    pub fn triangle_visible(&self, index: usize) -> bool {
        self.hidden.get(index).is_some_and(|h| !h)
    }

    /// Whether distance constraint `index` is active (false once torn).
    #[must_use]
    pub fn distance_active(&self, index: usize) -> bool {
        self.distance.get(index).is_some_and(|c| c.active)
    }

    /// Moves the links torn since the last call into `out` (ascending constraint index per
    /// step).
    pub fn drain_torn(&mut self, out: &mut Vec<ClothTorn>) {
        out.append(&mut self.torn_events);
    }

    /// Sets the solver's own wind velocity (m/s, clamped to 60 m/s when used).
    ///
    /// # Errors
    /// [`ClothError::NonFinite`].
    pub fn set_wind(&mut self, wind: Vec3) -> Result<(), ClothError> {
        self.wind = finite_vec(wind, "wind")?;
        Ok(())
    }

    /// The solver's own wind velocity.
    #[must_use]
    pub fn wind(&self) -> Vec3 {
        self.wind
    }

    /// The global wind term this solver feels: [`ClothSet::set_global_wind`]'s value, plus
    /// whatever a host bridge added for this solver alone (the game-world glue adds the scene
    /// wind scaled by the cloth's `wind_scale`, ADR-2884).
    #[must_use]
    pub fn global_wind(&self) -> Vec3 {
        self.global_wind
    }

    /// Sets this solver's global wind term (m/s). [`ClothSet::set_global_wind`] and
    /// [`ClothSet::insert`] overwrite it with the set's value; a host that adds a per-solver
    /// term writes it again every fixed step before the step.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`].
    pub fn set_global_wind(&mut self, wind: Vec3) -> Result<(), ClothError> {
        self.global_wind = finite_vec(wind, "wind")?;
        Ok(())
    }

    /// Gust modulation of the solver's own wind (`None` = steady).
    ///
    /// # Errors
    /// [`Gust::validate`]'s error.
    pub fn set_gust(&mut self, gust: Option<Gust>) -> Result<(), ClothError> {
        if let Some(g) = &gust {
            g.validate()?;
        }
        self.gust = gust;
        Ok(())
    }

    /// The wind that [`WindField`]s give this solver (set by the cloth plugin each fixed step,
    /// sampled at the solver's particle centroid).
    ///
    /// # Errors
    /// [`ClothError::NonFinite`].
    pub fn set_field_wind(&mut self, wind: Vec3) -> Result<(), ClothError> {
        self.field_wind = finite_vec(wind, "wind")?;
        Ok(())
    }

    /// The lifecycle state.
    #[must_use]
    pub fn state(&self) -> ClothState {
        self.state
    }

    /// Pauses (no stepping, state kept) or resumes. Resuming after more than
    /// [`lod::RESUME_RESET_STEPS`] paused fixed steps re-enters [`ClothState::Settling`].
    pub fn set_paused(&mut self, paused: bool) {
        match (paused, self.state) {
            (true, ClothState::Paused) | (false, ClothState::Running | ClothState::Settling) => {}
            (true, _) => {
                self.paused_steps = 0;
                self.set_state(ClothState::Paused);
            }
            (false, ClothState::Paused) => {
                if self.paused_steps > lod::RESUME_RESET_STEPS {
                    self.wake();
                } else {
                    self.set_state(ClothState::Running);
                }
            }
            (false, _) => {}
        }
    }

    /// Sets the distance LOD level (0..=2, or [`LOD_OFF`] to fade to the pose). Leaving off
    /// re-enters [`ClothState::Settling`].
    pub fn set_lod_level(&mut self, level: LodLevel) {
        let level = level.min(LOD_OFF);
        if level == self.lod {
            return;
        }
        let was_off = self.lod == LOD_OFF;
        self.lod = level;
        if level == LOD_OFF {
            if matches!(self.state, ClothState::Running | ClothState::Settling) {
                self.set_state(ClothState::Fading);
            }
        } else if was_off {
            self.wake();
        }
    }

    /// The distance LOD level.
    #[must_use]
    pub fn lod_level(&self) -> LodLevel {
        self.lod
    }

    /// Blended output positions and normals, one per particle (spec 55 § 2.2):
    /// `lerp(pose_target, simulated, blend * sim_mix)`, with area-weighted normals from the
    /// visible triangles of the blended positions (the pose normal, or zero, for particles in
    /// no triangle). Without a pose, the simulated positions. Reuses `out`'s allocation.
    pub fn output(&self, out: &mut Vec<PoseTarget>) {
        out.clear();
        let blend = self.blend();
        let bound = self.pose.cur.len() == self.particles.len();
        for (i, p) in self.particles.iter().enumerate() {
            let (position, normal) = if bound {
                let target = self.pose.cur[i];
                let mix = self.pose.maps.as_ref().map_or(1.0, |m| m.sim_mix_of(i));
                let f = blend * mix;
                let position = if f >= 1.0 {
                    p.position
                } else {
                    target.position.lerp_monotonic(p.position, f)
                };
                (position, target.normal)
            } else {
                (p.position, Vec3::ZERO)
            };
            out.push(PoseTarget { position, normal });
        }
        let mut accumulated = false;
        for (t, &hidden) in self.triangles.iter().zip(&self.hidden) {
            if hidden {
                continue;
            }
            if !accumulated {
                for o in out.iter_mut() {
                    o.normal = Vec3::ZERO;
                }
                accumulated = true;
            }
            let p = t.map(|i| out[i].position);
            let n = (p[1] - p[0]).cross(p[2] - p[0]);
            for &i in t {
                out[i].normal += n;
            }
        }
        if accumulated {
            for (i, o) in out.iter_mut().enumerate() {
                o.normal = match o.normal.try_normalize() {
                    Some(n) => n,
                    None if bound => self.pose.cur[i].normal,
                    None => Vec3::ZERO,
                };
            }
        }
    }

    /// Spec 55 state for tooling and the agent protocol.
    #[must_use]
    pub fn diagnostics(&self) -> ClothDiagnostics {
        let max_pose_error = if self.pose.has_targets() {
            self.particles
                .iter()
                .zip(&self.pose.cur)
                .filter(|(p, _)| p.inv_mass > 0.0)
                .map(|(p, t)| p.position.distance(t.position))
                .fold(0.0, f32::max)
        } else {
            0.0
        };
        ClothDiagnostics {
            state: self.state,
            blend: self.blend(),
            lod: self.lod,
            dropped_self_pairs: self.selfcol.dropped + self.vtcol.dropped,
            self_contacts: match self.effective_self_collision() {
                SelfCollision::VertexTriangle { .. } => self.vtcol.last_contacts,
                _ => self.selfcol.last_pairs,
            },
            torn: self.torn,
            wind: self.last_wind,
            max_pose_error,
            teleports: self.teleports,
            tethers: self.tethers.len(),
        }
    }

    /// Replaces the runaway guard settings (ADR-3273). Counters and the last-good copy stay.
    ///
    /// # Errors
    /// [`ClothGuard::validate`]'s error; nothing changes then.
    pub fn set_guard(&mut self, guard: ClothGuard) -> Result<(), ClothError> {
        guard.validate()?;
        if !guard.enabled {
            self.guard.good.clear();
            self.guard.streak = 0;
        }
        if guard.stiff_limit.is_some() && self.guard.settings.stiff_limit.is_none() {
            self.guard.tethers_valid = false;
        }
        self.guard.settings = guard;
        Ok(())
    }

    /// The runaway guard settings in use.
    #[must_use]
    pub fn guard(&self) -> ClothGuard {
        self.guard.settings
    }

    /// Guard counters and strain for tooling and the agent protocol (`physics.cloth_health`).
    #[must_use]
    pub fn health(&self) -> ClothHealth {
        ClothHealth {
            guard: self.guard.settings,
            max_strain: self.guard.max_strain,
            peak_strain: self.guard.peak_strain,
            faults: self.guard.faults,
            non_finite_faults: self.guard.non_finite,
            runaway_faults: self.guard.runaway,
            pose_resets: self.guard.pose_resets,
            last_fault: self.guard.last,
            extra_iterations: self.guard.extra_iterations,
            stiff_projections: self.guard.stiff_projections,
        }
    }

    /// Moves the guard faults recorded since the last call (at most [`FAULT_EVENTS_MAX`]; older
    /// ones are only counted) into `out`, oldest first.
    pub fn drain_faults(&mut self, out: &mut Vec<ClothFault>) {
        out.append(&mut self.guard.events);
    }

    /// Smallest distance between two self-collision candidate pairs of the last substep
    /// (`None` without self-collision or pairs).
    #[must_use]
    pub fn min_self_pair_distance(&self) -> Option<f32> {
        self.selfcol.min_pair_distance(&self.particles)
    }

    /// Counts and measures for tooling and the agent protocol.
    #[must_use]
    pub fn stats(&self) -> ClothStats {
        let max_stretch = self
            .distance
            .iter()
            .filter(|c| c.active && c.rest > EPSILON)
            .map(|c| (self.particles[c.a].position - self.particles[c.b].position).length() / c.rest)
            .fold(0.0, f32::max);
        ClothStats {
            particles: self.particles.len(),
            pinned: self.particles.iter().filter(|p| p.inv_mass == 0.0).count(),
            distance_constraints: self.distance.len(),
            bend_constraints: self.bends.len(),
            colliders: self.colliders.len(),
            steps: self.steps,
            dropped_steps: self.dropped_steps,
            contacts: self.last_contacts,
            max_stretch,
        }
    }

    /// A 64-bit FNV-1a digest of the solver state (spec 55 § 7): [`DIGEST_VERSION`], the step
    /// count, every particle's position, velocity and inverse mass bit for bit, the blend,
    /// ramp, state and LOD level, the current pose targets, the torn-link bitmap, the tuning
    /// and the wind terms. Two runs that agree bit for bit give the same digest; any drift
    /// changes it with overwhelming probability.
    #[must_use]
    pub fn state_digest(&self) -> u64 {
        let mut hash = Fnv1a::new();
        hash.write(&DIGEST_VERSION.to_le_bytes());
        hash.write(&self.steps.to_le_bytes());
        for p in &self.particles {
            for value in p.position.to_array().into_iter().chain(p.velocity.to_array()) {
                hash.write(&value.to_bits().to_le_bytes());
            }
            hash.write(&p.inv_mass.to_bits().to_le_bytes());
        }
        hash.write(&self.blend.to_bits().to_le_bytes());
        hash.write(&self.ramp.to_le_bytes());
        hash.write(&[self.state.tag(), self.lod]);
        for t in &self.pose.cur {
            for value in t.position.to_array().into_iter().chain(t.normal.to_array()) {
                hash.write(&value.to_bits().to_le_bytes());
            }
        }
        let mut byte = 0u8;
        for (i, c) in self.distance.iter().enumerate() {
            if !c.active {
                byte |= 1 << (i % 8);
            }
            if i % 8 == 7 {
                hash.write(&[byte]);
                byte = 0;
            }
        }
        hash.write(&[byte]);
        self.tuning.encode(&mut |b| hash.write(b));
        if let Some(model) = self.soft.as_deref() {
            model.encode(&mut |b| hash.write(b));
        }
        let winds = [self.wind, self.field_wind, self.global_wind];
        for v in winds.iter().flat_map(|w| w.to_array()) {
            hash.write(&v.to_bits().to_le_bytes());
        }
        // Garment layers (ADR-2425) only when set, so a solver without them keeps its digest.
        if !self.layers.is_empty() || self.layer_gap.is_some() {
            hash.write(b"layers");
            hash.write(&self.layers);
            hash.write(&self.layer_gap.unwrap_or(-1.0).to_bits().to_le_bytes());
        }
        hash.0
    }
}

// ---------------------------------------------------------------------------
// Constraint projection
// ---------------------------------------------------------------------------

/// XPBD's time-scaled compliance `α̃ = compliance / h²`, or `None` when the product overflows.
///
/// Compliance is validated finite and not negative, but a huge one times `1/h²` can still be
/// infinite, and `∞ · λ` is NaN at λ = 0 (each substep's start): one such constraint used to fill
/// the whole cloth with NaN (audit-v035-numerics M8). As `α̃ → ∞` the XPBD correction goes to
/// zero, so an overflowing constraint is skipped: exactly its limit. Finite products are
/// unchanged.
pub(crate) fn xpbd_alpha(compliance: f32, inv_h2: f32) -> Option<f32> {
    let alpha = compliance * inv_h2;
    alpha.is_finite().then_some(alpha)
}

/// XPBD distance constraint `C = |x_a - x_b| - rest` (Macklin et al. 2016, eq. 18).
///
/// Returns whether a zero-compliance link was longer than `stiff_limit` times its rest length
/// when the sweep reached it (the residual the adaptive iterations of ADR-3273 test); reading it
/// changes no arithmetic.
fn solve_distances(
    particles: &mut [ClothParticle],
    constraints: &mut [DistanceConstraint],
    inv_h2: f32,
    stiff_limit: f32,
) -> bool {
    let mut over = false;
    for c in constraints {
        let (pa, pb) = (particles[c.a], particles[c.b]);
        let w = pa.inv_mass + pb.inv_mass;
        if w == 0.0 || !c.active {
            continue;
        }
        let delta = pa.position - pb.position;
        let len = delta.length();
        if len <= EPSILON {
            continue;
        }
        // A zero-rest link (a weld) has no stretch ratio: like `strain_now` and the projection,
        // the limit skips it, or any gap would fire the extra iterations every substep
        // (corr-physics audit).
        if c.compliance == 0.0 && c.rest > EPSILON && len > stiff_limit * c.rest {
            over = true;
        }
        let n = delta / len;
        let Some(alpha) = xpbd_alpha(c.compliance, inv_h2) else {
            continue;
        };
        let dlambda = (-(len - c.rest) - alpha * c.lambda) / (w + alpha);
        c.lambda += dlambda;
        particles[c.a].position += n * (dlambda * pa.inv_mass);
        particles[c.b].position -= n * (dlambda * pb.inv_mass);
    }
    over
}

/// Dihedral angle of a hinge and the gradient of its cosine.
#[derive(Debug, Clone, Copy)]
struct Dihedral {
    /// Angle between the two triangle normals, in `[0, π]` (0 = flat).
    angle: f32,
    /// `sin(angle)`.
    sin: f32,
    /// Gradient of `cos(angle)` with respect to each of the four positions.
    grad_cos: [Vec3; 4],
}

/// The hinge `p[0]`–`p[1]` with wings `p[2]` and `p[3]`.
///
/// With `e = p1 - p0`, `n1 = norm(e × (p2 - p0))` and `n2 = norm((p3 - p0) × e)` (oriented so a
/// flat hinge has `n1 = n2`), `cos = n1 · n2`. Differentiating the normalized cross products
/// gives, with `A = (n2 - cos n1) / |e × r2|` and `B = (n1 - cos n2) / |r3 × e|`:
/// `∇p2 = A × e`, `∇p3 = e × B`, `∇p1 = r2 × A + B × r3`, and `∇p0` minus their sum.
/// `None` when either triangle is degenerate.
fn dihedral(p: [Vec3; 4]) -> Option<Dihedral> {
    let e = p[1] - p[0];
    let r2 = p[2] - p[0];
    let r3 = p[3] - p[0];
    let u1 = e.cross(r2);
    let u2 = r3.cross(e);
    let (l1, l2) = (u1.length(), u2.length());
    if !(l1 > EPSILON && l2 > EPSILON) {
        return None;
    }
    let (n1, n2) = (u1 / l1, u2 / l2);
    let cos = n1.dot(n2).clamp(-1.0, 1.0);
    let sin = n1.cross(n2).length();
    let a = (n2 - n1 * cos) / l1;
    let b = (n1 - n2 * cos) / l2;
    let g2 = a.cross(e);
    let g3 = e.cross(b);
    let g1 = r2.cross(a) + b.cross(r3);
    let g0 = -(g1 + g2 + g3);
    Some(Dihedral {
        angle: det_atan2(sin, cos),
        sin,
        grad_cos: [g0, g1, g2, g3],
    })
}

/// XPBD bending constraint `C = angle - rest_angle` (Müller et al. 2007 § 4.2, compliant
/// form of Macklin et al. 2016).
///
/// `∇C = -∇cos / sin`, so with `g = ∇cos`, `s = sin` and `W = Σ w |g|²` the XPBD update
/// `Δλ = (-C - α̃λ) / (W / s² + α̃)` and `Δx_i = w_i ∇C_i Δλ` are evaluated as
/// `Δλ = s² (-C - α̃λ) / (W + α̃ s²)` and `Δx_i = w_i g_i s (C + α̃λ) / (W + α̃ s²)`, which stay
/// finite as the hinge goes flat (`s → 0`).
fn solve_bends(particles: &mut [ClothParticle], constraints: &mut [BendConstraint], inv_h2: f32) {
    for c in constraints {
        let w = c.ids.map(|i| particles[i].inv_mass);
        if w.iter().all(|&wi| wi == 0.0) {
            continue;
        }
        let Some(hinge) = dihedral(c.ids.map(|i| particles[i].position)) else {
            continue;
        };
        let Some(alpha) = xpbd_alpha(c.compliance, inv_h2) else {
            continue;
        };
        let s = hinge.sin;
        let weight: f32 = w
            .iter()
            .zip(&hinge.grad_cos)
            .map(|(wi, g)| wi * g.length_squared())
            .sum();
        let denom = weight + alpha * s * s;
        if denom <= EPSILON * EPSILON {
            continue;
        }
        let rhs = (hinge.angle - c.rest_angle) + alpha * c.lambda;
        c.lambda += -s * s * rhs / denom;
        let scale = s * rhs / denom;
        for ((&id, wi), g) in c.ids.iter().zip(w).zip(hinge.grad_cos) {
            particles[id].position += g * (wi * scale);
        }
    }
}

/// Pushes free particles out of every collider (to `thickness` outside) and records the
/// contact for friction. `from` and `to` are the colliders at the start and the end of the
/// substep (equal for static colliders); the surface velocity at the contact is their
/// difference over `h`.
fn solve_collisions(
    particles: &mut [ClothParticle],
    contacts: &mut [Contact],
    previous: &[Vec3],
    from: &[ClothCollider],
    to: &[ClothCollider],
    frame: snapshot::SubstepFrame,
) {
    let (thickness, h) = (frame.thickness, frame.h);
    if to.is_empty() {
        return;
    }
    for ((p, contact), &prev) in particles.iter_mut().zip(contacts.iter_mut()).zip(previous) {
        if p.inv_mass == 0.0 {
            continue;
        }
        for (start, collider) in from.iter().zip(to) {
            let (distance, normal) = collider.signed_distance(p.position);
            if distance < thickness {
                let push = thickness - distance;
                p.position += normal * push;
                contact.depth += push;
                contact.normal = normal;
                contact.friction = collider.friction;
                contact.velocity = surface_velocity(start, collider, p.position, h);
            } else if let Some((target, normal)) =
                swept_collider(start, collider, prev, p.position, thickness)
            {
                contact.depth += (target - p.position).length();
                p.position = target;
                contact.normal = normal;
                contact.friction = collider.friction;
                contact.velocity = surface_velocity(start, collider, p.position, h);
            }
        }
    }
}

/// Swept test of a fast collider (spec 55 § 2.4): when the collider's surface near `x` moved
/// more than `thickness` during the substep (`start` to `end`), the particle's path from `prev`
/// to `x` is sampled against the collider moving along with it. At the first sample that enters
/// the shape the particle is pushed out to the thickness and carried with the collider to its
/// end pose. Returns the new position and the contact normal, or `None` when the collider is
/// slow or the paths never met (the end test already found nothing).
fn swept_collider(
    start: &ClothCollider,
    end: &ClothCollider,
    prev: Vec3,
    x: Vec3,
    thickness: f32,
) -> Option<(Vec3, Vec3)> {
    let travel = surface_velocity(start, end, x, 1.0).length();
    if thickness.is_nan() || thickness <= 0.0 || travel.is_nan() || travel <= thickness {
        return None;
    }
    let samples = ((travel / thickness).ceil() as u32).clamp(2, SWEEP_SAMPLES_MAX);
    let mut was_outside = start.signed_distance(prev).0 >= thickness;
    for j in 1..=samples {
        let t = j as f32 / samples as f32;
        let shape = lerp_collider(start, end, t);
        let at = prev.lerp(x, t);
        let (distance, normal) = shape.signed_distance(at);
        let inside = distance < thickness;
        if inside && was_outside {
            let out = at + normal * (thickness - distance);
            let mut target = out + surface_velocity(&shape, end, out, 1.0);
            let (d1, n1) = end.signed_distance(target);
            if d1 < thickness {
                target += n1 * (thickness - d1);
            }
            return Some((target, n1));
        }
        was_outside = !inside;
    }
    None
}

/// Pushes free particles out of every world-snapshot shape (in key order) and records the
/// contacts like [`solve_collisions`].
///
/// With `reaction` (one linear and one angular slot per snapshot shape) each push also adds the
/// impulse the shape receives, `-m * push / h`, and its moment about the shape's current origin
/// (two-way reaction, spec 55 § 2.4).
fn solve_snapshot(
    particles: &mut [ClothParticle],
    contacts: &mut [Contact],
    previous: &[Vec3],
    snap: &WorldSnapshot,
    frame: snapshot::SubstepFrame,
    mut reaction: Option<(&mut [Vec3], &mut [Vec3])>,
) {
    for ((p, contact), &prev) in particles.iter_mut().zip(contacts.iter_mut()).zip(previous) {
        if p.inv_mass == 0.0 {
            continue;
        }
        let impulse_scale = if frame.h > 0.0 {
            -1.0 / (p.inv_mass * frame.h)
        } else {
            0.0
        };
        for index in 0..snap.shapes().len() {
            let mut slot = reaction
                .as_mut()
                .and_then(|(linear, angular)| Some((linear.get_mut(index)?, angular.get_mut(index)?)));
            let origin = snap.shapes()[index].cur.translation;
            snap.collide_shape(index, p.position, prev, frame, |hit| {
                if let Some((linear, angular)) = slot.as_mut() {
                    let impulse = hit.push * impulse_scale;
                    **linear += impulse;
                    **angular += (p.position - origin).cross(impulse);
                }
                p.position += hit.push;
                contact.depth += hit.push.length();
                contact.normal = hit.normal;
                contact.friction = hit.friction;
                contact.velocity = hit.velocity;
            });
        }
    }
}

/// Velocity of `to`'s surface near `x`, from its shape `from` one substep `h` earlier.
fn surface_velocity(from: &ClothCollider, to: &ClothCollider, x: Vec3, h: f32) -> Vec3 {
    let moved = match (from.shape, to.shape) {
        (ClothShape::Sphere { center: c0, .. }, ClothShape::Sphere { center: c1, .. }) => c1 - c0,
        (ClothShape::Capsule { a: a0, b: b0, .. }, ClothShape::Capsule { a: a1, b: b1, .. }) => {
            let ab = b1 - a1;
            let len2 = ab.length_squared();
            let t = if len2 > EPSILON {
                ((x - a1).dot(ab) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (a1 + ab * t) - (a0 + (b0 - a0) * t)
        }
        (ClothShape::Plane { offset: o0, .. }, ClothShape::Plane { normal, offset: o1 }) => {
            normal * (o1 - o0)
        }
        _ => Vec3::ZERO,
    };
    if h > 0.0 { moved / h } else { Vec3::ZERO }
}

/// `prev` moved toward `cur` by `t` (same shape kind; a kind change jumps to `cur`).
fn lerp_collider(prev: &ClothCollider, cur: &ClothCollider, t: f32) -> ClothCollider {
    let shape = match (prev.shape, cur.shape) {
        (
            ClothShape::Sphere {
                center: c0,
                radius: r0,
            },
            ClothShape::Sphere {
                center: c1,
                radius: r1,
            },
        ) => ClothShape::Sphere {
            center: c0.lerp(c1, t),
            radius: r0 + (r1 - r0) * t,
        },
        (
            ClothShape::Capsule {
                a: a0,
                b: b0,
                radius: r0,
            },
            ClothShape::Capsule {
                a: a1,
                b: b1,
                radius: r1,
            },
        ) => ClothShape::Capsule {
            a: a0.lerp(a1, t),
            b: b0.lerp(b1, t),
            radius: r0 + (r1 - r0) * t,
        },
        (
            ClothShape::Plane {
                normal: n0,
                offset: o0,
            },
            ClothShape::Plane {
                normal: n1,
                offset: o1,
            },
        ) => ClothShape::Plane {
            normal: n0.lerp(n1, t).try_normalize().unwrap_or(n1),
            offset: o0 + (o1 - o0) * t,
        },
        (_, shape) => shape,
    };
    ClothCollider {
        shape,
        friction: cur.friction,
    }
}

/// Long-range attachments (Kim et al. 2012): a free particle farther than `scale × path
/// length` from its anchor is moved back onto that sphere.
fn solve_tethers(particles: &mut [ClothParticle], tethers: &[Tether], scale: f32) {
    if !scale.is_finite() {
        return;
    }
    for t in tethers {
        let anchor = particles[t.anchor].position;
        let p = &mut particles[t.particle];
        if p.inv_mass == 0.0 {
            continue;
        }
        let limit = t.length * scale;
        let d = p.position - anchor;
        let len = d.length();
        if len > limit && len > EPSILON {
            p.position = anchor + d * (limit / len);
        }
    }
}

/// Sweeps over the active links (at most [`STRAIN_CAP_SWEEPS`] times, stopping when no link is
/// outside the cap) and moves every link whose length ratio is outside `[cap.min, cap.max]` back
/// to the nearest bound, split by inverse mass. One Gauss-Seidel sweep leaves chains under load
/// stretched, hence the repeats; on long loaded chains the sweeps still converge slowly, so the
/// cap bounds links locally and tethers bound the overall length.
fn solve_strain_caps(particles: &mut [ClothParticle], constraints: &[DistanceConstraint], cap: StrainCap) {
    for _ in 0..STRAIN_CAP_SWEEPS {
        let mut violated = false;
        let mut project = |c: &DistanceConstraint| {
            if !c.active || c.rest <= EPSILON {
                return;
            }
            let (pa, pb) = (particles[c.a], particles[c.b]);
            let w = pa.inv_mass + pb.inv_mass;
            if w == 0.0 {
                return;
            }
            let delta = pa.position - pb.position;
            let len = delta.length();
            if len <= EPSILON {
                return;
            }
            let ratio = len / c.rest;
            let target = if ratio > cap.max {
                cap.max * c.rest
            } else if ratio < cap.min {
                cap.min * c.rest
            } else {
                return;
            };
            violated = true;
            let n = delta / len;
            let corr = (len - target) / w;
            particles[c.a].position -= n * (corr * pa.inv_mass);
            particles[c.b].position += n * (corr * pb.inv_mass);
        };
        constraints.iter().for_each(&mut project);
        if !violated {
            break;
        }
    }
}

/// The long-range attachments of ADR-3273: like `solve_tethers` at `scale`. A move adds velocity
/// like any projection, up to `max_move` (the attachment speed limit times the substep): the part
/// of a longer move (a sheet hauled after its pin was teleported) is also applied to `previous`,
/// so it adds no speed.
fn solve_guard_tethers(
    particles: &mut [ClothParticle],
    previous: &mut [Vec3],
    tethers: &[Tether],
    scale: f32,
    max_move: f32,
) {
    for t in tethers {
        let anchor = particles[t.anchor].position;
        let p = &mut particles[t.particle];
        if p.inv_mass == 0.0 {
            continue;
        }
        let limit = t.length * scale;
        let d = p.position - anchor;
        let len = d.length();
        if len > limit && len > EPSILON {
            let moved = anchor + d * (limit / len);
            let step = moved - p.position;
            let travel = step.length();
            if travel > max_move {
                previous[t.particle] += step * (1.0 - max_move / travel);
            }
            p.position = moved;
        }
    }
}

/// The stiff-link projection of ADR-3273: up to [`STRAIN_CAP_SWEEPS`] sweeps (stopping when none
/// is over) move every active zero-compliance link longer than `limit` times its rest length
/// back to that length, split by inverse mass. A correction adds velocity like any projection's
/// up to `max_move`; the excess of a longer one is applied to `previous` too. Only runs when the
/// last sweep of a substep still found such a link, so a converged solver never reaches it.
fn solve_stiff_limit(
    particles: &mut [ClothParticle],
    previous: &mut [Vec3],
    constraints: &[DistanceConstraint],
    limit: f32,
    max_move: f32,
) {
    for _ in 0..STRAIN_CAP_SWEEPS {
        let mut violated = false;
        for c in constraints {
            if !c.active || c.compliance != 0.0 || c.rest <= EPSILON {
                continue;
            }
            let (pa, pb) = (particles[c.a], particles[c.b]);
            let w = pa.inv_mass + pb.inv_mass;
            if w == 0.0 {
                continue;
            }
            let delta = pa.position - pb.position;
            let len = delta.length();
            let target = limit * c.rest;
            if len <= target || len.is_nan() {
                continue;
            }
            violated = true;
            let corr = (len - target) / w;
            let n = delta / len;
            let (move_a, move_b) = (n * (corr * pa.inv_mass), n * (corr * pb.inv_mass));
            particles[c.a].position -= move_a;
            particles[c.b].position += move_b;
            let travel = corr * pa.inv_mass.max(pb.inv_mass);
            if travel > max_move {
                let keep = 1.0 - max_move / travel;
                previous[c.a] -= move_a * keep;
                previous[c.b] += move_b * keep;
            }
        }
        if !violated {
            break;
        }
    }
}

/// Velocity update for a particle that touched a collider this substep (Müller et al. 2007
/// § 3.5: friction and restitution act on the velocities of colliding vertices).
///
/// - Inelastic contact: the outward normal speed after the contact is at most the outward
///   speed the particle had before the substep (`predicted`), and never inward. Without this,
///   pushing a particle out of a collider that moved onto it would turn the push into launch
///   speed.
/// - Coulomb friction: the tangential speed drops by at most
///   `friction × (normal correction / h)`, so a particle sticks while the tangential pull stays
///   below `friction` times the normal one.
fn contact_velocity(velocity: Vec3, predicted: Vec3, contact: &Contact, h: f32) -> Vec3 {
    // Work relative to the collider's surface (zero for static colliders).
    let surface = contact.velocity;
    let velocity = velocity - surface;
    let predicted = predicted - surface;
    let n = contact.normal;
    let vn = velocity.dot(n);
    let vn = vn.min(predicted.dot(n).max(0.0)).max(0.0);
    let tangent = velocity - n * velocity.dot(n);
    let speed = tangent.length();
    let limit = contact.friction * contact.depth / h;
    let tangent = if speed <= limit || speed <= EPSILON {
        Vec3::ZERO
    } else {
        tangent * (1.0 - limit / speed)
    };
    tangent + n * vn + surface
}

/// FNV-1a, 64-bit (Fowler–Noll–Vo, public domain): stable across Rust releases and platforms.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

// ---------------------------------------------------------------------------
// ECS integration
// ---------------------------------------------------------------------------

/// Named cloth solvers stepped by [`ClothPlugin`] and read by the agent protocol
/// (`physics.cloth_stats`). Names are unique; insertion order is the stepping order.
#[derive(Resource, Debug, Clone, Default)]
pub struct ClothSet {
    entries: Vec<(String, ClothSolver)>,
    global_wind: Vec3,
    parallel: bool,
}

impl ClothSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// [`ClothSolver::shift_origin`] on every solver.
    pub fn shift_all(&mut self, shift: Vec3) {
        for (_, solver) in &mut self.entries {
            solver.shift_origin(shift);
        }
    }

    /// Adds `solver` under `name`, replacing (and returning) a solver with the same name.
    pub fn insert(&mut self, name: impl Into<String>, mut solver: ClothSolver) -> Option<ClothSolver> {
        let name = name.into();
        solver.global_wind = self.global_wind;
        if let Some((_, slot)) = self.entries.iter_mut().find(|(n, _)| *n == name) {
            return Some(std::mem::replace(slot, solver));
        }
        self.entries.push((name, solver));
        None
    }

    /// Removes and returns the solver named `name`.
    pub fn remove(&mut self, name: &str) -> Option<ClothSolver> {
        let index = self.entries.iter().position(|(n, _)| n == name)?;
        Some(self.entries.remove(index).1)
    }

    /// The solver named `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ClothSolver> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// The solver named `name`, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut ClothSolver> {
        self.entries.iter_mut().find(|(n, _)| n == name).map(|(_, s)| s)
    }

    /// Index of the solver named `name` (one scan). [`Self::get_slot`] is O(1) after that.
    #[must_use]
    pub fn slot_of(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|(n, _)| n == name)
    }

    /// Name stored at `index`.
    #[must_use]
    pub fn slot_name(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(|(n, _)| n.as_str())
    }

    /// The solver at `index`.
    #[must_use]
    pub fn get_slot(&self, index: usize) -> Option<&ClothSolver> {
        self.entries.get(index).map(|(_, solver)| solver)
    }

    /// The solver at `index`, mutably.
    pub fn get_slot_mut(&mut self, index: usize) -> Option<&mut ClothSolver> {
        self.entries.get_mut(index).map(|(_, solver)| solver)
    }

    /// Every `(name, solver)` pair in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ClothSolver)> {
        self.entries.iter().map(|(n, s)| (n.as_str(), s))
    }

    /// Number of solvers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set holds no solver.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sets the wind every solver feels on top of its own and the fields' (m/s).
    ///
    /// # Errors
    /// [`ClothError::NonFinite`].
    pub fn set_global_wind(&mut self, wind: Vec3) -> Result<(), ClothError> {
        self.global_wind = finite_vec(wind, "wind")?;
        for (_, solver) in &mut self.entries {
            solver.global_wind = self.global_wind;
        }
        Ok(())
    }

    /// The global wind.
    #[must_use]
    pub fn global_wind(&self) -> Vec3 {
        self.global_wind
    }

    /// Every `(name, solver)` pair in insertion order, mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut ClothSolver)> {
        self.entries.iter_mut().map(|(n, s)| (n.as_str(), s))
    }

    /// Steps every solver by `dt` in insertion order and returns the total number of fixed
    /// steps run.
    ///
    /// # Errors
    /// [`ClothError::NonFinite`] or [`ClothError::Negative`] for `dt` (checked before any
    /// solver runs).
    ///
    /// With [`ClothSet::set_parallel`] on, the solvers step on the compute task pool, one
    /// solver per task (a fixed partition that does not depend on the thread count); each solver
    /// is single-threaded and reads only its own state, and the per-solver step counts are
    /// summed in insertion order afterwards, so the result is bit-identical to the serial run.
    pub fn step_all(&mut self, dt: f32) -> Result<u32, ClothError> {
        non_negative(dt, "dt")?;
        if self.parallel && self.entries.len() > 1 {
            let _span = debug_span!("cloth_step_parallel", solvers = self.entries.len()).entered();
            let counts = regia_core::tasks::par_chunks_mut(&mut self.entries, 1, |_, chunk| {
                chunk
                    .iter_mut()
                    .map(|(_, solver)| solver.step(dt))
                    .collect::<Result<Vec<u32>, ClothError>>()
            });
            // Deterministic reduction: insertion order, first error wins.
            let mut total = 0u32;
            for chunk in counts {
                for ran in chunk? {
                    total = total.saturating_add(ran);
                }
            }
            return Ok(total);
        }
        let mut total = 0u32;
        for (_, solver) in &mut self.entries {
            total = total.saturating_add(solver.step(dt)?);
        }
        Ok(total)
    }

    /// Steps the solvers in parallel (`true`) or one after another (`false`) in
    /// [`ClothSet::step_all`]. `false` is the [`ClothSet::new`] default until [`ClothPlugin`]
    /// inserts a set and turns parallel on. Both give bit-identical results; parallel pays off
    /// with several busy solvers.
    pub fn set_parallel(&mut self, parallel: bool) {
        self.parallel = parallel;
    }

    /// Whether [`ClothSet::step_all`] steps the solvers in parallel.
    #[must_use]
    pub fn parallel(&self) -> bool {
        self.parallel
    }
}

/// Steps every solver in [`ClothSet`] by one `FixedTime` step (installed by [`ClothPlugin`]).
pub fn step_cloth_set(fixed: Res<FixedTime>, mut set: ResMut<ClothSet>) {
    if set.is_empty() {
        return;
    }
    let _span = debug_span!("step_cloth_set", solvers = set.len()).entered();
    let dt = fixed.step().as_secs_f32();
    if let Err(err) = set.step_all(dt) {
        warn!(target: "regia_physics", "cloth step skipped: {err}");
    }
}

/// Samples every [`WindField`] at each solver's particle centroid and sets the sum as the
/// solver's field wind. The fold order is the same on every host and every run: fields with an
/// `EntityGuid` first in full 128-bit guid order, then fields without one in `SpawnOrder`
/// arrival order (the key `sync_added_bodies` uses, ADR-1347) — never entity-id or query
/// order.
pub fn gather_cloth_wind(
    fields: Query<(
        Entity,
        &WindField,
        &regia_scene::GlobalTransform,
        Option<&regia_scene::prelude::EntityGuid>,
    )>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<WindField>>>,
    mut set: ResMut<ClothSet>,
    mut keyed: Local<Vec<(WindSortKey, WindField, Vec3, regia_core::math::Quat)>>,
) {
    if set.is_empty() {
        return;
    }
    let _span = debug_span!("cloth_wind_gather").entered();
    keyed.clear();
    for (entity, field, transform, guid) in &fields {
        let (_, rotation, translation) = transform.0.to_scale_rotation_translation();
        keyed.push((
            wind_sort_key(guid, order.as_deref(), entity),
            *field,
            translation,
            rotation,
        ));
    }
    keyed.sort_by_key(|k| k.0);
    for (_, solver) in set.iter_mut() {
        let at = solver.centroid();
        let step = solver.stats().steps;
        let sum: Vec3 = keyed
            .iter()
            .map(|(_, field, origin, rotation)| field.sample(*origin, *rotation, at, step))
            .fold(Vec3::ZERO, |a, b| a + b);
        // Samples are finite by construction; a failure keeps the previous value.
        let _ = solver.set_field_wind(sum);
    }
}

/// Sort key of one wind field: guid-bearing entities first, ordered by the full 128-bit guid;
/// guid-less entities after, ordered by `SpawnOrder` arrival (falling back to the entity id
/// for entities the hook never saw, as `step::spawn_key` defines).
type WindSortKey = (u8, u128, regia_ecs::spawn_order::SpawnKey);

fn wind_sort_key(
    guid: Option<&regia_scene::prelude::EntityGuid>,
    order: Option<&regia_ecs::spawn_order::SpawnOrder<WindField>>,
    entity: Entity,
) -> WindSortKey {
    match guid {
        Some(g) => (0, g.0.as_u128(), (0, 0, 0)),
        None => (1, 0, crate::step::spawn_key(order, entity)),
    }
}

/// Inserts an empty [`ClothSet`] and [`SoftSet`] (keeping ones the app already has),
/// registers [`WindField`] for reflection, and steps both in `FixedUpdate` after the
/// rigid-body step (`PhysicsStep`) when that plugin is present: wind gather, then cloth, then
/// soft bodies.
pub struct ClothPlugin;

impl Plugin for ClothPlugin {
    fn build(&self, app: &mut App) {
        // Wind fields fold in spawn order (see `gather_cloth_wind`): keep that order
        // independent of entity ids (ADR-1347), as bodies and colliders already do.
        regia_ecs::spawn_order::track_spawn_order::<WindField>(app.world_mut());
        if !app.world().contains_resource::<ClothSet>() {
            let mut set = ClothSet::new();
            set.set_parallel(true);
            app.insert_resource(set);
        }
        app.init_resource::<SoftSet>()
            .register_type::<WindField>()
            .register_type::<WindFieldKind>()
            .add_systems(
                FixedUpdate,
                (gather_cloth_wind, step_cloth_set, soft::step_soft_set)
                    .chain()
                    .after(crate::plugin::PhysicsStep),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::system::RunSystemOnce;

    fn hinge(fold: f32) -> [Vec3; 4] {
        // Shared edge along X; wing c in +Z, wing d in -Z lifted by `fold`.
        [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.4, 0.1, 0.8),
            Vec3::new(0.6, fold, -0.7),
        ]
    }

    fn cos_of(p: [Vec3; 4]) -> f32 {
        let e = p[1] - p[0];
        let n1 = e.cross(p[2] - p[0]).normalize();
        let n2 = (p[3] - p[0]).cross(e).normalize();
        n1.dot(n2)
    }

    #[test]
    fn a_zero_rest_stiff_link_does_not_fire_the_stiff_limit() {
        // Corr-physics audit: `len > stiff_limit * rest` is `len > 0` for a weld (rest 0), which
        // the projection skips, so every substep ran the extra iterations and the projection.
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        let p0 = solver.add_particle(Vec3::new(0.0, 1.0, 0.0), 1.0).expect("p0");
        let p1 = solver.add_particle(Vec3::new(0.5, 1.0, 0.0), 1.0).expect("p1");
        let p2 = solver.add_particle(Vec3::new(0.5, 1.0, 0.0), 1.0).expect("p2");
        solver.pin(p0).expect("pin");
        solver.add_distance(p0, p1, 0.0).expect("link");
        solver.add_distance(p1, p2, 0.0).expect("weld");
        solver
            .set_velocity(p2, Vec3::new(0.0, 0.0, 1.0))
            .expect("velocity");
        solver.set_guard(ClothGuard::default()).expect("guard");
        solver.step_fixed(1);
        let health = solver.health();
        assert_eq!(health.extra_iterations, 0, "{health:?}");
        assert_eq!(health.stiff_projections, 0, "{health:?}");
    }

    /// corr-low: `strain_now` returned +inf when a link's squared length overflowed f32, though
    /// `ClothFault::strain` documents the largest finite ratio (0 when none was finite).
    #[test]
    fn the_strain_readout_stays_finite_when_a_squared_length_overflows() {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        let p0 = solver.add_particle(Vec3::ZERO, 0.0).expect("p0");
        let p1 = solver.add_particle(Vec3::X, 0.0).expect("p1");
        solver.add_distance(p0, p1, 0.0).expect("link");
        // The distance constraint keeps rest 1; the particle itself is moved past the point
        // where (3e20)² overflows f32.
        solver.particles[p1].position = Vec3::new(3.0e20, 0.0, 0.0);
        assert_eq!(solver.strain_now(), 0.0);
    }

    #[test]
    fn pose_targets_keep_their_allocation() {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        solver
            .add_grid(&ClothGrid {
                origin: Vec3::ZERO,
                u_axis: Vec3::X,
                v_axis: Vec3::Z,
                columns: 8,
                rows: 4,
                spacing: 0.1,
                particle_mass: 0.05,
                stretch_compliance: 0.0,
                shear_compliance: 0.0,
                bend_compliance: 0.0,
            })
            .expect("grid");
        let n = solver.particles().len();
        let targets: Vec<PoseTarget> = (0..n)
            .map(|i| PoseTarget {
                position: Vec3::new(i as f32 * 0.1, 1.0, 0.0),
                normal: Vec3::Y,
            })
            .collect();
        solver.set_pose_targets(&targets, &targets).expect("first write");
        solver.pose.prev.reserve(n + 32);
        solver.pose.cur.reserve(n + 32);
        let prev_cap = solver.pose.prev.capacity();
        let cur_cap = solver.pose.cur.capacity();
        assert!(prev_cap >= n + 32 && cur_cap >= n + 32);
        let shifted: Vec<PoseTarget> = targets
            .iter()
            .map(|target| PoseTarget {
                position: target.position + Vec3::X,
                normal: Vec3::Y,
            })
            .collect();
        solver.set_pose_targets(&targets, &shifted).expect("second write");
        assert_eq!(
            solver.pose.prev.capacity(),
            prev_cap,
            "prev was replaced instead of reused"
        );
        assert_eq!(
            solver.pose.cur.capacity(),
            cur_cap,
            "cur was replaced instead of reused"
        );
        assert!(solver.pose.prev.capacity() >= n && solver.pose.cur.capacity() >= n);
        assert_eq!(solver.pose.cur[0].position, shifted[0].position);
        assert_eq!(solver.pose.prev[1].position, targets[1].position);
        let bad = vec![PoseTarget::default(); n.saturating_sub(1)];
        assert!(solver.set_pose_targets(&bad, &targets).is_err());
        assert_eq!(solver.pose.prev.capacity(), prev_cap);
        assert_eq!(solver.pose.cur[0].position, shifted[0].position);
    }

    #[test]
    fn flat_hinge_has_zero_angle() {
        let flat = [
            Vec3::ZERO,
            Vec3::X,
            Vec3::new(0.5, 0.0, 1.0),
            Vec3::new(0.5, 0.0, -1.0),
        ];
        let h = dihedral(flat).expect("not degenerate");
        assert!(h.angle.abs() < 1e-6, "angle {}", h.angle);
        assert!(h.sin.abs() < 1e-6);
        // Folding the wing up by 90 degrees gives a right angle.
        let folded = [
            Vec3::ZERO,
            Vec3::X,
            Vec3::new(0.5, 0.0, 1.0),
            Vec3::new(0.5, 1.0, 0.0),
        ];
        let h = dihedral(folded).expect("not degenerate");
        assert!(
            (h.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-6,
            "angle {}",
            h.angle
        );
    }

    #[test]
    fn degenerate_hinge_is_skipped() {
        let collinear = [
            Vec3::ZERO,
            Vec3::X,
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(0.5, 0.0, 1.0),
        ];
        assert!(dihedral(collinear).is_none());
    }

    #[test]
    fn dihedral_gradient_matches_finite_differences() {
        for fold in [0.3_f32, -0.5, 0.9] {
            let p = hinge(fold);
            let h = dihedral(p).expect("not degenerate");
            let step = 1e-3_f32;
            for (vertex, grad) in h.grad_cos.iter().enumerate() {
                for axis in 0..3 {
                    let mut plus = p;
                    let mut minus = p;
                    plus[vertex][axis] += step;
                    minus[vertex][axis] -= step;
                    let numeric = (cos_of(plus) - cos_of(minus)) / (2.0 * step);
                    assert!(
                        (numeric - grad[axis]).abs() < 2e-3,
                        "fold {fold} vertex {vertex} axis {axis}: numeric {numeric} analytic {}",
                        grad[axis]
                    );
                }
            }
            // Translation invariance.
            let sum: Vec3 = h.grad_cos.iter().copied().sum();
            assert!(sum.length() < 1e-5);
        }
    }

    #[test]
    fn bend_projection_restores_the_rest_angle() {
        let mut particles: Vec<ClothParticle> = hinge(0.0)
            .into_iter()
            .map(|position| ClothParticle {
                position,
                velocity: Vec3::ZERO,
                inv_mass: 1.0,
            })
            .collect();
        let rest = dihedral(hinge(0.0)).expect("hinge").angle;
        particles[3].position.y = 0.6;
        let mut constraints = [BendConstraint {
            ids: [0, 1, 2, 3],
            rest_angle: rest,
            compliance: 0.0,
            lambda: 0.0,
        }];
        for _ in 0..50 {
            solve_bends(&mut particles, &mut constraints, 1.0);
        }
        let ids = constraints[0].ids;
        let angle = dihedral(ids.map(|i| particles[i].position)).expect("hinge").angle;
        assert!((angle - rest).abs() < 1e-3, "angle {angle} rest {rest}");
    }

    #[test]
    fn contact_clamps_tangential_speed_and_removes_inward_speed() {
        let contact = Contact {
            depth: 0.01,
            normal: Vec3::Y,
            friction: 0.5,
            velocity: Vec3::ZERO,
        };
        // limit = 0.5 * 0.01 / 0.01 = 0.5 m/s.
        let v = contact_velocity(Vec3::new(2.0, -1.0, 0.0), Vec3::ZERO, &contact, 0.01);
        assert!((v - Vec3::new(1.5, 0.0, 0.0)).length() < 1e-6, "{v}");
        // Below the limit the particle sticks; it was already leaving at 0.2 m/s, which it keeps.
        let v = contact_velocity(Vec3::new(0.3, 0.2, 0.0), Vec3::new(0.0, 0.5, 0.0), &contact, 0.01);
        assert!((v - Vec3::new(0.0, 0.2, 0.0)).length() < 1e-6, "{v}");
    }

    #[test]
    fn contact_does_not_launch_a_pushed_out_particle() {
        let contact = Contact {
            depth: 0.1,
            normal: Vec3::Y,
            friction: 0.0,
            velocity: Vec3::ZERO,
        };
        // A collider moved onto a resting particle: the 0.1 m push would read as 10 m/s.
        let v = contact_velocity(Vec3::new(0.0, 10.0, 0.0), Vec3::ZERO, &contact, 0.01);
        assert_eq!(v, Vec3::ZERO);
        // A particle already moving outward at 3 m/s keeps 3 m/s, not the push's 10 m/s.
        let v = contact_velocity(
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::new(0.0, 3.0, 0.0),
            &contact,
            0.01,
        );
        assert_eq!(v, Vec3::new(0.0, 3.0, 0.0));
    }

    #[test]
    fn cloth_set_names_are_unique_and_ordered() {
        let mut set = ClothSet::new();
        let solver = ClothSolver::new(ClothConfig::default()).expect("config");
        assert!(set.insert("flag", solver.clone()).is_none());
        assert!(set.insert("rope", solver.clone()).is_none());
        assert!(set.insert("flag", solver).is_some(), "same name replaces");
        let names: Vec<&str> = set.iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["flag", "rope"]);
        assert!(set.get_mut("rope").is_some());
        assert!(set.remove("flag").is_some());
        assert!(set.get("flag").is_none());
        assert_eq!(set.len(), 1);
        assert_eq!(set.step_all(f32::NAN), Err(ClothError::NonFinite { what: "dt" }));
    }

    #[test]
    fn step_system_advances_every_solver_by_one_fixed_step() {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        solver.add_particle(Vec3::ZERO, 1.0).expect("particle");
        let mut set = ClothSet::new();
        set.insert("a", solver.clone());
        set.insert("b", solver);
        let mut world = World::new();
        world.insert_resource(FixedTime::default());
        world.insert_resource(set);
        world.run_system_once(step_cloth_set).expect("system runs");
        let set = world.resource::<ClothSet>();
        for (name, solver) in set.iter() {
            assert_eq!(solver.stats().steps, 1, "{name}");
            assert!(solver.particles()[0].position.y < 0.0, "{name} fell");
        }
    }

    #[test]
    fn plugin_inserts_an_empty_set() {
        let mut app = App::new();
        app.add_plugins(ClothPlugin);
        assert!(app.world().resource::<ClothSet>().is_empty());
        assert!(app.world().resource::<ClothSet>().parallel());
        assert!(app.world().resource::<SoftSet>().is_empty());

        let mut app = App::new();
        app.insert_resource(ClothSet::new());
        app.add_plugins(ClothPlugin);
        assert!(!app.world().resource::<ClothSet>().parallel());
    }

    #[test]
    fn wind_fields_reach_every_solver_in_key_order() {
        use regia_core::math::Affine3A;
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        solver
            .add_particle(Vec3::new(0.0, 1.0, 0.0), 1.0)
            .expect("particle");
        let mut set = ClothSet::new();
        set.insert("flag", solver);
        set.set_global_wind(Vec3::new(1.0, 0.0, 0.0)).expect("global");
        let mut world = World::new();
        world.insert_resource(set);
        world.spawn((
            WindField {
                kind: WindFieldKind::Directional { speed: 3.0 },
                radius: None,
                gust: None,
            },
            regia_scene::GlobalTransform(Affine3A::IDENTITY),
        ));
        world.spawn((
            WindField {
                kind: WindFieldKind::Radial {
                    strength: 2.0,
                    falloff: 0.0,
                },
                radius: Some(10.0),
                gust: None,
            },
            regia_scene::GlobalTransform(Affine3A::from_translation(Vec3::ZERO)),
        ));
        world.run_system_once(gather_cloth_wind).expect("system runs");
        let mut set = world.resource_mut::<ClothSet>();
        let solver = set.get_mut("flag").expect("flag");
        // Directional along -Z (3 m/s) plus radial away from the origin (+Y, 2 m/s).
        assert!(
            (solver.field_wind - Vec3::new(0.0, 2.0, -3.0)).length() < 1e-5,
            "{}",
            solver.field_wind
        );
        solver.step_fixed(1);
        let wind = solver.diagnostics().wind;
        assert!((wind - Vec3::new(1.0, 2.0, -3.0)).length() < 1e-5, "{wind}");
    }

    // --- ADR-4773 determinism pins (audit G finding 1): the hinge angle the solver integrates
    // and the rest angle the cook stores must be bit-identical on every host, so both come from
    // the deterministic atan2 kernel. `hinge(0.3)`'s angle, pinned:
    const HINGE_0_3_ANGLE_BITS: u32 = 1_057_455_288;

    #[test]
    fn hinge_angle_bits_are_platform_independent() {
        let h = dihedral(hinge(0.3)).expect("not degenerate");
        assert_eq!(
            h.angle.to_bits(),
            HINGE_0_3_ANGLE_BITS,
            "the bend residual is `angle - rest_angle`; platform libm atan2 must not feed it"
        );
    }

    #[test]
    fn rest_pose_hinge_does_not_move() {
        let p = hinge(0.3);
        let rest = dihedral(p).expect("hinge").angle;
        let mut particles: Vec<ClothParticle> = p
            .iter()
            .map(|&position| ClothParticle {
                position,
                velocity: Vec3::ZERO,
                inv_mass: 1.0,
            })
            .collect();
        let before: Vec<_> = particles.iter().map(|p| p.position).collect();
        let mut constraints = [BendConstraint {
            ids: [0, 1, 2, 3],
            rest_angle: rest,
            compliance: 0.0,
            lambda: 0.0,
        }];
        solve_bends(&mut particles, &mut constraints, 1.0);
        let after: Vec<_> = particles.iter().map(|p| p.position).collect();
        assert_eq!(
            before, after,
            "a hinge sitting exactly on its cooked rest pose has a zero residual on every host"
        );
    }

    // --- ADR-4773 determinism pins (audit G finding 3): the field wind fold order. With speeds
    // (1, 1e-4, -0.9999) the two association orders give different f32 bits, so the pinned fold
    // proves the sum ran in arrival order, not entity-id or query order.
    const SPAWN_ORDER_FOLD_BITS: u32 = 3_109_142_528; // -0.0002: the fields blow along -Z

    fn wind_sum_world(build: impl FnOnce(&mut World, WindField, WindField, WindField)) -> Vec3 {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        solver
            .add_particle(Vec3::new(0.0, 1.0, 0.0), 1.0)
            .expect("particle");
        let mut set = ClothSet::new();
        set.insert("flag", solver);
        let mut world = World::new();
        world.insert_resource(set);
        // The plugin installs this before any WindField exists; same contract here.
        assert!(regia_ecs::spawn_order::track_spawn_order::<WindField>(&mut world));
        let field = |speed: f32| WindField {
            kind: WindFieldKind::Directional { speed },
            radius: None,
            gust: None,
        };
        build(&mut world, field(1.0), field(1.0e-4), field(-0.9999));
        world.run_system_once(gather_cloth_wind).expect("system runs");
        world.resource::<ClothSet>().get("flag").expect("flag").field_wind
    }

    #[test]
    fn wind_sum_follows_spawn_order_not_entity_ids() {
        use regia_core::math::Affine3A;
        // Direct spawns: entity ids follow arrival order.
        let direct = wind_sum_world(|world, x, a, b| {
            for field in [x, a, b] {
                world.spawn((field, regia_scene::GlobalTransform(Affine3A::IDENTITY)));
            }
        });
        // Three command queues, as parallel systems leave them: the queues reserve ids in the
        // order A < X < B (the record order of parallel systems is timing-dependent) but apply
        // in the order X, A, B, so arrival order (what SpawnOrder records, ADR-1347) differs
        // from the entity-id order.
        let queued = wind_sum_world(|world, x, a, b| {
            let ident = || regia_scene::GlobalTransform(Affine3A::IDENTITY);
            let mut qx = regia_ecs::ecs::world::CommandQueue::default();
            let mut qa = regia_ecs::ecs::world::CommandQueue::default();
            let mut qb = regia_ecs::ecs::world::CommandQueue::default();
            let _ = Commands::new(&mut qa, &*world).spawn((a, ident())).id();
            let _ = Commands::new(&mut qx, &*world).spawn((x, ident())).id();
            let _ = Commands::new(&mut qb, &*world).spawn((b, ident())).id();
            qx.apply(world);
            qa.apply(world);
            qb.apply(world);
        });
        assert_eq!(
            direct.z.to_bits(),
            SPAWN_ORDER_FOLD_BITS,
            "arrival-order fold ((1 + 1e-4) - 0.9999), along the fields' forward (-Z)"
        );
        assert_eq!(direct.x.to_bits(), 0);
        assert_eq!(direct.y.to_bits(), 0);
        assert_eq!(
            queued, direct,
            "the sum must not depend on how entity ids were assigned"
        );
    }

    #[test]
    fn wind_sort_key_prefers_full_guid_then_arrival() {
        use regia_core::id::Guid;
        use regia_ecs::spawn_order::SpawnOrder;
        use regia_scene::prelude::EntityGuid;
        let mut world = World::new();
        assert!(regia_ecs::spawn_order::track_spawn_order::<WindField>(&mut world));
        // Arrival order beats entity ids for guid-less fields.
        let low = world.spawn_empty().id();
        let high = world.spawn_empty().id();
        world.entity_mut(high).insert(WindField::default());
        world.entity_mut(low).insert(WindField::default());
        let order = world.resource::<SpawnOrder<WindField>>();
        assert!(
            wind_sort_key(None, Some(order), low) > wind_sort_key(None, Some(order), high),
            "the later-arriving low id sorts after the earlier-arriving high id"
        );
        // Guids order by the full 128 bits: two guids colliding in the low 64 still sort.
        let g1 = EntityGuid(Guid::from_u128(0x0001_0000_0000_0000_0000_0000_0000_0005));
        let g2 = EntityGuid(Guid::from_u128(0x0002_0000_0000_0000_0000_0000_0000_0005));
        assert!(
            wind_sort_key(Some(&g1), None, low) < wind_sort_key(Some(&g2), None, low),
            "full-128-bit guid order, not the truncated low 64"
        );
        // A guid beats any guid-less field, whatever the entity ids.
        assert!(
            wind_sort_key(Some(&g2), None, low) < wind_sort_key(None, Some(order), high),
            "guid fields fold before guid-less fields"
        );
    }
}
