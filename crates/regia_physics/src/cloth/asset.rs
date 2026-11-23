// SPDX-License-Identifier: MPL-2.0
// Original code (spec 55 § 4). Closest point on a triangle: the Voronoi-region test from
// C. Ericson, "Real-Time Collision Detection", Morgan Kaufmann 2005, § 5.1.5 (re-derived).

//! Cloth and soft-body assets (spec 55 § 4): the `.cloth.ron` and `.soft.ron` sources, the
//! cooked [`ClothAsset`] / [`SoftAsset`] they become, and the cooked `.cloth.bin` /
//! `.soft.bin` payloads (little-endian, 4-byte aligned, wrapped in the standard `FCKD` cooked
//! header by the cooker).
//!
//! Cooking is split in two so the runtime crate owns every rule a loader checks:
//!
//! - the desktop cooker (`regia_asset_pipeline::cloth_cook`) resolves the glTF meshes a source
//!   names into [`SimMeshInput`] / [`RenderMeshInput`] and calls [`ClothAsset::cook`] /
//!   [`SoftAsset::cook`];
//! - [`ClothAsset::cook`] validates the source, derives the constraints from the sim mesh
//!   triangles, expands the vertex maps, quantizes the joint weights and binds every render
//!   vertex to a sim triangle.
//!
//! [`ClothAsset::to_bytes`] / [`ClothAsset::from_bytes`] write and read the payload; cooking the
//! same source twice gives the same bytes. [`ClothAsset::build_solver`] makes a
//! [`ClothSolver`] for one LOD.
//!
//! # Constraints from a triangle mesh
//!
//! - `Stretch`: one distance link per unique triangle edge.
//! - `Shear`: for every interior edge (shared by exactly two triangles) a distance link between
//!   the two opposite vertices.
//! - `Bend`: a dihedral bend on every interior edge (hinge = the edge, wings = the opposite
//!   vertices).
//! - `Tether`: switches on the solver's tethers (`ClothParams::tether`, default scale 1).
//! - `Volume`: soft bodies only (`.soft.ron`); a cloth asset with it fails to cook.
//!
//! Runs are ordered by kind (stretch, shear, bend), then by `(a, b)`.

use std::collections::BTreeMap;

use regia_core::math::{Affine3A, Vec3, det_atan2};
use serde::{Deserialize, Serialize};

use super::soft::{SoftModel, SoftParams, SoftSolver};
use super::tuning::{ClothTuning, SelfCollision, StrainCap, TearParams};
use super::wind::WindModel;
use super::{ClothConfig, ClothError, ClothSolver, PoseMaps};

/// Version of the `.cloth.ron` source format.
pub const CLOTH_FORMAT_VERSION: u32 = 1;
/// Version of the `.soft.ron` source format.
pub const SOFT_FORMAT_VERSION: u32 = 1;
/// Version of the cooked `.cloth.bin` payload.
pub const COOKED_CLOTH_FORMAT_VERSION: u32 = 1;
/// Version of the cooked `.soft.bin` payload.
pub const COOKED_SOFT_FORMAT_VERSION: u32 = 1;
/// Type path stored in the cooked header of a `.cloth.bin`.
pub const COOKED_CLOTH_TYPE_PATH: &str = "regia_physics::cloth::ClothAsset";
/// Type path stored in the cooked header of a `.soft.bin`.
pub const COOKED_SOFT_TYPE_PATH: &str = "regia_physics::cloth::SoftAsset";
/// Most particles one cloth or soft asset may have.
pub const MAX_ASSET_PARTICLES: usize = 65_535;
/// Most substeps a cooked cloth or soft body runs on Android (spec 55 § 9). A game's
/// `ClothSim.params` is used as given.
pub const ANDROID_SUBSTEPS: u32 = 4;
/// The vertex-map names a `.cloth.ron` may use (spec 55 § 2.2 plus `layer`).
pub const MAP_NAMES: [&str; 6] = ["reach", "standoff", "pose_pull", "sim_mix", "mass_scale", "layer"];

/// Magic at the start of a cooked cloth payload (inside the `FCKD` wrapper).
const CLOTH_PAYLOAD_MAGIC: [u8; 4] = *b"RCLO";
/// Magic at the start of a cooked soft payload (inside the `FCKD` wrapper).
const SOFT_PAYLOAD_MAGIC: [u8; 4] = *b"RSOF";

/// Mass given to a particle that no triangle touches, in kilograms.
const LONE_PARTICLE_MASS: f32 = 1e-3;

fn invalid(reason: impl Into<String>) -> ClothError {
    ClothError::AssetInvalid {
        reason: reason.into(),
    }
}

// ---------------------------------------------------------------------------
// Source formats
// ---------------------------------------------------------------------------

/// A mesh inside a glTF file: the file (relative to the source file) and the node name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshRef {
    /// glTF file, relative to the `.cloth.ron`.
    pub source: String,
    /// Name of the node whose mesh is used.
    pub node: String,
}

/// The render mesh a cloth's visible section comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderMeshRef {
    /// glTF file, relative to the `.cloth.ron`.
    pub source: String,
    /// Name of the node whose mesh is used.
    pub node: String,
    /// Primitive index of the cloth section inside that mesh.
    #[serde(default)]
    pub section: u32,
}

/// A per-vertex float array in a source file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MapData {
    /// One value per sim vertex.
    Dense(Vec<f32>),
    /// A default plus explicit `(vertex, value)` entries.
    Sparse {
        /// Value of every vertex not listed.
        default: f32,
        /// Explicit values.
        #[serde(default)]
        entries: Vec<(u32, f32)>,
    },
}

impl MapData {
    /// The dense array for `len` vertices.
    ///
    /// # Errors
    /// [`ClothError::MapSizeMismatch`] for a dense map of the wrong length or a sparse entry
    /// past the end; [`ClothError::NonFinite`] for NaN or infinity.
    pub fn expand(&self, len: usize, map: &'static str) -> Result<Vec<f32>, ClothError> {
        let values = match self {
            Self::Dense(values) => {
                if values.len() != len {
                    return Err(ClothError::MapSizeMismatch {
                        map,
                        len: values.len(),
                        expected: len,
                    });
                }
                values.clone()
            }
            Self::Sparse { default, entries } => {
                let mut values = vec![*default; len];
                for &(i, v) in entries {
                    let slot = values.get_mut(i as usize).ok_or(ClothError::MapSizeMismatch {
                        map,
                        len: i as usize + 1,
                        expected: len,
                    })?;
                    *slot = v;
                }
                values
            }
        };
        if values.iter().any(|v| !v.is_finite()) {
            return Err(ClothError::NonFinite { what: map });
        }
        Ok(values)
    }
}

/// What a [`ConstraintSet`] builds (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ConstraintKind {
    /// Distance links along the triangle edges.
    Stretch,
    /// Distance links across interior edges.
    Shear,
    /// Dihedral bends on interior edges.
    Bend,
    /// Long-range tethers to the zero-reach vertices.
    Tether,
    /// Volume constraints (soft bodies only).
    Volume,
}

impl ConstraintKind {
    fn code(self) -> u8 {
        match self {
            Self::Stretch => 0,
            Self::Shear => 1,
            Self::Bend => 2,
            Self::Tether => 3,
            Self::Volume => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Stretch,
            1 => Self::Shear,
            2 => Self::Bend,
            3 => Self::Tether,
            4 => Self::Volume,
            _ => return None,
        })
    }
}

/// One named constraint set of a `.cloth.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConstraintSet {
    /// Name (shown by tools).
    pub name: String,
    /// What the set builds.
    pub kind: ConstraintKind,
    /// Compliance of every constraint in the set.
    pub compliance: f32,
    /// Optional per-vertex compliance scale; a constraint uses the mean over its vertices.
    #[serde(default)]
    pub scale: Option<MapData>,
}

/// Hard cap on edge length ratios in a source file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StrainCapParams {
    /// Largest `length / rest` (at least 1).
    pub max: f32,
    /// Smallest `length / rest` (in `0..=1`).
    pub min: f32,
}

/// Tearing settings in a source file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TearSettings {
    /// Length ratio above which a link is cut (greater than 1).
    pub strain: f32,
    /// Most links cut per fixed step (at least 1).
    pub max_per_step: u32,
}

/// Self-collision mode in a source file.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum SelfCollisionSetting {
    /// None.
    #[default]
    Off,
    /// Particle spheres of `radius` meters.
    Spheres {
        /// Sphere radius in meters.
        radius: f32,
    },
    /// Vertex against triangle within `radius` meters (ADR-2425).
    VertexTriangle {
        /// Contact radius in meters.
        radius: f32,
    },
}

/// Wind response in a source file.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WindSetting {
    /// Flat-plate drag and lift.
    Aerodynamic {
        /// Drag coefficient.
        drag: f32,
        /// Lift coefficient.
        lift: f32,
    },
    /// Acceleration `wind * gain` on every free particle.
    Push {
        /// Gain in 1/s.
        gain: f32,
    },
}

impl Default for WindSetting {
    fn default() -> Self {
        Self::Aerodynamic { drag: 1.0, lift: 0.3 }
    }
}

/// Cloth parameters (spec 55 § 2.3): asset defaults, overridable per instance. Every field has
/// a default, so a source lists only what it changes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClothParams {
    /// Scales gravity (-4..4).
    pub gravity_scale: f32,
    /// Replaces the solver gravity when set.
    pub gravity_override: Option<[f32; 3]>,
    /// Velocity damping in 1/s (0..20).
    pub damping: f32,
    /// Compliance of the stretch links (default set; cook time only, ADR-2250).
    pub stretch_compliance: f32,
    /// Compliance of the shear links (default set; cook time only, ADR-2250).
    pub shear_compliance: f32,
    /// Compliance of the bends (default set; cook time only, ADR-2250).
    pub bend_compliance: f32,
    /// Substeps per fixed step.
    pub substeps: u32,
    /// Iterations per substep.
    pub iterations: u32,
    /// Pose-pull compliance.
    pub pull_compliance: f32,
    /// Root inertia kept (0..1).
    pub inertia_scale: f32,
    /// Optional strain cap.
    pub strain_cap: Option<StrainCapParams>,
    /// Optional tether scale (at least 1).
    pub tether: Option<f32>,
    /// Collision thickness in meters.
    pub collision_thickness: f32,
    /// Validated (0..2) but not yet read by the solver: contacts use the collider's friction
    /// (ADR-2250).
    pub friction: f32,
    /// Self-collision mode.
    pub self_collision: SelfCollisionSetting,
    /// Self-collision friction (0..2).
    pub self_friction: f32,
    /// Wind response.
    pub wind_model: WindSetting,
    /// Air density in kg/m³.
    pub air_density: f32,
    /// Teleport distance in meters.
    pub teleport_distance: f32,
    /// Optional tearing.
    pub tear: Option<TearSettings>,
    /// Fabric area density in kg/m²: a particle's mass is a third of the area of its triangles
    /// times this (then times its `mass_scale`).
    pub density: f32,
    /// How much of the scene wind (`regia_scene::weather::SceneWind`, spec 99 § 3.3) this cloth
    /// feels (0..=10; 1 = all of it, 0 = none). Read by the game-world glue, not the solver.
    /// Optional in files: absent means 1, and 1 is not written, so files and cooked bytes of
    /// cloths that never set it are unchanged (ADR-2884).
    #[serde(skip_serializing_if = "is_unit_wind_scale")]
    pub wind_scale: f32,
}

/// Whether `v` is the default [`ClothParams::wind_scale`] (serde skips writing it).
fn is_unit_wind_scale(v: &f32) -> bool {
    v.to_bits() == 1.0f32.to_bits()
}

impl Default for ClothParams {
    fn default() -> Self {
        Self {
            gravity_scale: 1.0,
            gravity_override: None,
            damping: 0.1,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
            substeps: 8,
            iterations: 1,
            pull_compliance: 1e-3,
            inertia_scale: 1.0,
            strain_cap: None,
            tether: None,
            collision_thickness: 0.01,
            friction: 0.3,
            self_collision: SelfCollisionSetting::Off,
            self_friction: 0.2,
            wind_model: WindSetting::default(),
            air_density: 1.2,
            teleport_distance: 2.0,
            tear: None,
            density: 0.2,
            wind_scale: 1.0,
        }
    }
}

impl ClothParams {
    /// These parameters on a platform: on Android authored substeps are capped at
    /// [`ANDROID_SUBSTEPS`] (spec 55 § 9); cooked assets are the same on every platform.
    #[must_use]
    pub fn for_platform(mut self, android: bool) -> Self {
        if android {
            self.substeps = self.substeps.min(ANDROID_SUBSTEPS);
        }
        self
    }

    /// The solver settings these parameters give at fixed step `time_step`.
    #[must_use]
    pub fn config(&self, time_step: f32) -> ClothConfig {
        ClothConfig {
            time_step,
            substeps: self.substeps,
            iterations: self.iterations,
            damping: self.damping,
            thickness: self.collision_thickness,
            ..ClothConfig::default()
        }
    }

    /// The solver tuning these parameters give.
    #[must_use]
    pub fn tuning(&self) -> ClothTuning {
        ClothTuning {
            gravity_scale: self.gravity_scale,
            gravity_override: self.gravity_override.map(Vec3::from_array),
            pull_compliance: self.pull_compliance,
            inertia_scale: self.inertia_scale,
            strain_cap: self.strain_cap.map(|c| StrainCap {
                max: c.max,
                min: c.min,
            }),
            tether: self.tether,
            self_collision: match self.self_collision {
                SelfCollisionSetting::Off => SelfCollision::Off,
                SelfCollisionSetting::Spheres { radius } => SelfCollision::Spheres { radius },
                SelfCollisionSetting::VertexTriangle { radius } => SelfCollision::VertexTriangle { radius },
            },
            self_friction: self.self_friction,
            wind_model: match self.wind_model {
                WindSetting::Aerodynamic { drag, lift } => WindModel::Aerodynamic { drag, lift },
                WindSetting::Push { gain } => WindModel::Push { gain },
            },
            air_density: self.air_density,
            teleport_distance: self.teleport_distance,
            tear: self.tear.map(|t| TearParams {
                strain: t.strain,
                max_per_step: t.max_per_step,
            }),
            ..ClothTuning::default()
        }
    }

    /// Checks every field (the solver's config and tuning rules, the damping range, a positive
    /// density and friction in `0..=2`).
    ///
    /// # Errors
    /// The first offending field as a [`ClothError`].
    pub fn validate(&self) -> Result<(), ClothError> {
        self.config(1.0 / 60.0).validate()?;
        self.tuning().validate()?;
        let checks: [(&'static str, f32, f32, f32); 7] = [
            ("damping", self.damping, 0.0, 20.0),
            ("wind_scale", self.wind_scale, 0.0, 10.0),
            ("friction", self.friction, 0.0, 2.0),
            ("stretch_compliance", self.stretch_compliance, 0.0, f32::MAX),
            ("shear_compliance", self.shear_compliance, 0.0, f32::MAX),
            ("bend_compliance", self.bend_compliance, 0.0, f32::MAX),
            ("density", self.density, f32::MIN_POSITIVE, f32::MAX),
        ];
        for (what, value, lo, hi) in checks {
            if !value.is_finite() {
                return Err(ClothError::NonFinite { what });
            }
            if !(lo..=hi).contains(&value) {
                return Err(ClothError::BadParam { what });
            }
        }
        Ok(())
    }

    fn apply(&self, overrides: &ClothParamOverrides) -> Self {
        let mut out = *self;
        if let Some(v) = overrides.damping {
            out.damping = v;
        }
        if let Some(v) = overrides.substeps {
            out.substeps = v;
        }
        if let Some(v) = overrides.iterations {
            out.iterations = v;
        }
        if let Some(v) = overrides.stretch_compliance {
            out.stretch_compliance = v;
        }
        if let Some(v) = overrides.shear_compliance {
            out.shear_compliance = v;
        }
        if let Some(v) = overrides.bend_compliance {
            out.bend_compliance = v;
        }
        if let Some(v) = overrides.collision_thickness {
            out.collision_thickness = v;
        }
        if let Some(v) = overrides.self_collision {
            out.self_collision = v;
        }
        if let Some(v) = overrides.wind_model {
            out.wind_model = v;
        }
        out
    }
}

/// The parameters a sim-mesh LOD may override (everything else comes from LOD 0).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClothParamOverrides {
    /// Velocity damping.
    pub damping: Option<f32>,
    /// Substeps.
    pub substeps: Option<u32>,
    /// Iterations.
    pub iterations: Option<u32>,
    /// Stretch compliance.
    pub stretch_compliance: Option<f32>,
    /// Shear compliance.
    pub shear_compliance: Option<f32>,
    /// Bend compliance.
    pub bend_compliance: Option<f32>,
    /// Collision thickness.
    pub collision_thickness: Option<f32>,
    /// Self-collision mode.
    pub self_collision: Option<SelfCollisionSetting>,
    /// Wind response.
    pub wind_model: Option<WindSetting>,
}

/// One sim-mesh LOD (LOD 1 and up) of a `.cloth.ron`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ClothLodSource {
    /// A coarser sim mesh; `None` keeps LOD 0's mesh (only the overrides change).
    #[serde(default)]
    pub sim_mesh: Option<MeshRef>,
    /// Parameter overrides.
    #[serde(default)]
    pub overrides: ClothParamOverrides,
}

/// A `.cloth.ron` source file (spec 55 § 4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClothSource {
    /// Must be at most [`CLOTH_FORMAT_VERSION`].
    pub format_version: u32,
    /// Asset name.
    pub name: String,
    /// The skeleton the cloth is skinned to (`None`: the cloth follows its entity).
    #[serde(default)]
    pub skeleton: Option<String>,
    /// The sim mesh.
    pub sim_mesh: MeshRef,
    /// The visible section; `None` draws the sim mesh itself.
    #[serde(default)]
    pub render_mesh: Option<RenderMeshRef>,
    /// Parameters.
    #[serde(default)]
    pub params: ClothParams,
    /// Optional named starting point (`light_fabric`, `silk`, ...; see
    /// [`ClothPreset`](super::presets::ClothPreset)). Fields of `params` still at their default
    /// take the preset's numbers; fields the author changed win. `None` (the default) keeps the
    /// pre-preset behavior, so older files read unchanged (ADR-2439).
    #[serde(default)]
    pub preset: Option<String>,
    /// Vertex maps by name ([`MAP_NAMES`]).
    #[serde(default)]
    pub maps: BTreeMap<String, MapData>,
    /// Constraint sets; empty means stretch, shear and bend with the parameter compliances.
    #[serde(default)]
    pub constraint_sets: Vec<ConstraintSet>,
    /// Names of the body proxies the cloth collides with (empty: every proxy under the same
    /// skinned root).
    #[serde(default)]
    pub proxies: Vec<String>,
    /// LOD 1 and up.
    #[serde(default)]
    pub lods: Vec<ClothLodSource>,
}

#[derive(Deserialize)]
struct VersionOnly {
    format_version: u32,
}

fn read_version(text: &str, current: u32) -> Result<(), ClothError> {
    let probe: VersionOnly = ron::from_str(text).map_err(|e| invalid(format!("format_version: {e}")))?;
    if probe.format_version > current {
        return Err(ClothError::UnsupportedVersion {
            found: probe.format_version,
            supported: current,
        });
    }
    if probe.format_version == 0 {
        return Err(invalid("format_version 0"));
    }
    Ok(())
}

impl ClothSource {
    /// This source with its `preset` (if any) folded into `params` and cleared, as the cooker
    /// reads it.
    ///
    /// # Errors
    /// [`ClothError::AssetInvalid`] (reason starting `unknown_cloth_preset`) for a name no
    /// preset has.
    pub fn with_preset_resolved(&self) -> Result<Self, ClothError> {
        let mut out = self.clone();
        if let Some(name) = &self.preset {
            let preset = super::presets::ClothPreset::from_name(name).map_err(|e| invalid(e.to_string()))?;
            out.params = preset.under(&self.params);
            out.preset = None;
        }
        Ok(out)
    }

    /// Parses a `.cloth.ron` and checks its version, name and map names.
    ///
    /// # Errors
    /// [`ClothError::UnsupportedVersion`] for a newer file, [`ClothError::AssetInvalid`] for
    /// a parse error, an empty name or an unknown map name.
    pub fn from_ron_str(text: &str) -> Result<Self, ClothError> {
        read_version(text, CLOTH_FORMAT_VERSION)?;
        let source: Self = ron::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if let Some(name) = &source.preset {
            super::presets::ClothPreset::from_name(name).map_err(|e| invalid(e.to_string()))?;
        }
        if source.name.trim().is_empty() {
            return Err(invalid("empty name"));
        }
        if let Some(name) = source.maps.keys().find(|k| !MAP_NAMES.contains(&k.as_str())) {
            return Err(invalid(format!("unknown map {name:?}")));
        }
        Ok(source)
    }

    /// The canonical text of this source (pretty RON; maps in name order).
    ///
    /// # Errors
    /// [`ClothError::AssetInvalid`] when serialization fails.
    pub fn to_ron_string(&self) -> Result<String, ClothError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty()).map_err(|e| invalid(e.to_string()))
    }
}

/// The soft-body model of a `.soft.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SoftModelSource {
    /// A closed surface held at `pressure` times its rest volume.
    Pressure {
        /// Target volume ratio.
        pressure: f32,
        /// Volume compliance (0 = incompressible).
        volume_compliance: f32,
    },
    /// A tetrahedral mesh (from the tet generator or listed here).
    Tetra {
        /// Tetrahedra; empty means "generate from the closed surface at `cell`".
        #[serde(default)]
        tets: Vec<[u32; 4]>,
        /// Voxel size in meters for the tet generator (used when `tets` is empty).
        #[serde(default)]
        cell: Option<f32>,
        /// Volume compliance (0 = incompressible).
        volume_compliance: f32,
    },
    /// Shape-matching clusters.
    ShapeMatch {
        /// Particle indices per cluster (empty: one cluster of every vertex).
        #[serde(default)]
        clusters: Vec<Vec<u32>>,
        /// Pull toward the goal, `0..=1`.
        stiffness: f32,
        /// Linear blend, `0..=1`.
        #[serde(default)]
        beta: f32,
    },
}

/// Soft-body parameters in a `.soft.ron`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoftParamsSource {
    /// Substeps per fixed step.
    pub substeps: u32,
    /// Iterations per substep.
    pub iterations: u32,
    /// Velocity damping in 1/s.
    pub damping: f32,
    /// Mass of every particle in kilograms.
    pub particle_mass: f32,
    /// Compliance of the edge links.
    pub edge_compliance: f32,
    /// Collision thickness in meters.
    pub collision_thickness: f32,
    /// Scales gravity.
    pub gravity_scale: f32,
}

impl Default for SoftParamsSource {
    fn default() -> Self {
        let base = SoftParams::default();
        Self {
            substeps: base.config.substeps,
            iterations: base.config.iterations,
            damping: base.config.damping,
            particle_mass: base.particle_mass,
            edge_compliance: base.edge_compliance,
            collision_thickness: base.config.thickness,
            gravity_scale: 1.0,
        }
    }
}

impl SoftParamsSource {
    /// These parameters on a platform (see [`ClothParams::for_platform`]).
    #[must_use]
    pub fn for_platform(mut self, android: bool) -> Self {
        if android {
            self.substeps = self.substeps.min(ANDROID_SUBSTEPS);
        }
        self
    }

    /// The solver parameters at fixed step `time_step`.
    #[must_use]
    pub fn params(&self, time_step: f32) -> SoftParams {
        let config = ClothConfig {
            time_step,
            substeps: self.substeps,
            iterations: self.iterations,
            damping: self.damping,
            thickness: self.collision_thickness,
            ..ClothConfig::default()
        };
        SoftParams {
            config: ClothConfig {
                gravity: config.gravity * self.gravity_scale,
                ..config
            },
            particle_mass: self.particle_mass,
            edge_compliance: self.edge_compliance,
        }
    }
}

/// A `.soft.ron` source file (spec 55 § 4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoftSource {
    /// Must be at most [`SOFT_FORMAT_VERSION`].
    pub format_version: u32,
    /// Asset name.
    pub name: String,
    /// The volume model.
    pub model: SoftModelSource,
    /// The closed surface (also the render mesh).
    pub surface: MeshRef,
    /// Parameters.
    #[serde(default)]
    pub params: SoftParamsSource,
    /// Vertex maps (`reach`, `pose_pull`, `mass_scale`), one value per particle.
    #[serde(default)]
    pub maps: BTreeMap<String, MapData>,
}

impl SoftSource {
    /// Parses a `.soft.ron` and checks its version, name and map names.
    ///
    /// # Errors
    /// As [`ClothSource::from_ron_str`].
    pub fn from_ron_str(text: &str) -> Result<Self, ClothError> {
        read_version(text, SOFT_FORMAT_VERSION)?;
        let source: Self = ron::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if source.name.trim().is_empty() {
            return Err(invalid("empty name"));
        }
        if let Some(name) = source.maps.keys().find(|k| !MAP_NAMES.contains(&k.as_str())) {
            return Err(invalid(format!("unknown map {name:?}")));
        }
        Ok(source)
    }
}

/// The serde field and variant names of the `.cloth.ron` format, for `cargo xtask check-formats`.
#[must_use]
pub fn cloth_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<ClothSource>(),
        serde_field_names::<MeshRef>(),
        serde_field_names::<RenderMeshRef>(),
        serde_field_names::<ClothParams>(),
        serde_field_names::<StrainCapParams>(),
        serde_field_names::<TearSettings>(),
        serde_field_names::<ConstraintSet>(),
        serde_field_names::<ClothLodSource>(),
        serde_field_names::<ClothParamOverrides>(),
        serde_variant_names::<MapData>(),
        serde_variant_names::<ConstraintKind>(),
        serde_variant_names::<SelfCollisionSetting>(),
        serde_variant_names::<WindSetting>(),
    ])
}

/// The serde field and variant names of the `.soft.ron` format.
#[must_use]
pub fn soft_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<SoftSource>(),
        serde_field_names::<MeshRef>(),
        serde_field_names::<SoftParamsSource>(),
        serde_variant_names::<SoftModelSource>(),
        serde_variant_names::<MapData>(),
    ])
}

/// A description of the cooked `.cloth.bin` payload layout (it is not serde): bump
/// [`COOKED_CLOTH_FORMAT_VERSION`] when this changes.
#[must_use]
pub fn cooked_cloth_schema_fingerprint() -> String {
    format!(
        "RCLO;v{COOKED_CLOTH_FORMAT_VERSION};header:u32x4(particles,triangles,lods,flags);name;proxies;\
         lod:[rest f32x3;triangles u32x3;constraints(u8 kind,u8,u16,u32 a,u32 b,u32 c,u32 d,f32 rest,f32 compliance);\
         maps(u32 len,f32*) x6;skin(u8x4,u8x4);render(u32 tri,f32 u,f32 v,f32 d,f32x2 uv;u32x3 indices);params ron]"
    )
}

/// A description of the cooked `.soft.bin` payload layout.
#[must_use]
pub fn cooked_soft_schema_fingerprint() -> String {
    format!(
        "RSOF;v{COOKED_SOFT_FORMAT_VERSION};name;rest f32x3;model(u8 kind:pressure tris|tetra tets|shape clusters);\
         surface(u32 tet,f32x4 w;f32x2 uv;u32x3 indices);maps(u32 len,f32*) x3;skin(u8x4,u8x4);params ron"
    )
}

fn cooked_payload_version(bytes: &[u8], type_path: &str, magic: [u8; 4]) -> Result<Option<u32>, String> {
    let Ok((header, payload)) = regia_asset::decode_cooked_file(bytes) else {
        return Ok(None);
    };
    if header.type_tag != regia_core::id::TypePathHash::of(type_path) {
        return Ok(None);
    }
    if payload.len() < 8 || payload[..4] != magic {
        return Err("cooked payload has a bad magic".to_string());
    }
    Ok(Some(u32::from_le_bytes([
        payload[4], payload[5], payload[6], payload[7],
    ])))
}

/// The payload of a whole cooked file of type `type_path`.
fn cooked_payload<'a>(bytes: &'a [u8], type_path: &str, what: &str) -> Result<&'a [u8], ClothError> {
    let (header, payload) = regia_asset::decode_cooked_file(bytes).map_err(|e| invalid(e.to_string()))?;
    if header.type_tag != regia_core::id::TypePathHash::of(type_path) {
        return Err(invalid(what));
    }
    Ok(payload)
}

/// The payload version of a cooked `.cloth.bin` file (`FCKD` wrapper), `Ok(None)` when the file
/// is another cooked type (for `regia upgrade-project`).
///
/// # Errors
/// A cloth file whose payload magic is wrong.
pub fn cooked_cloth_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    cooked_payload_version(bytes, COOKED_CLOTH_TYPE_PATH, CLOTH_PAYLOAD_MAGIC)
}

/// The payload version of a cooked `.soft.bin` file, `Ok(None)` for other cooked types.
///
/// # Errors
/// A soft file whose payload magic is wrong.
pub fn cooked_soft_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    cooked_payload_version(bytes, COOKED_SOFT_TYPE_PATH, SOFT_PAYLOAD_MAGIC)
}

/// The `from` versions of the `.cloth.ron` migration steps (none yet).
#[must_use]
pub fn cloth_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The `from` versions of the `.soft.ron` migration steps (none yet).
#[must_use]
pub fn soft_migration_sources() -> Vec<u32> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Cooker input and cooked data
// ---------------------------------------------------------------------------

/// A sim mesh as the cooker resolved it from glTF (model space).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SimMeshInput {
    /// Vertex positions.
    pub positions: Vec<Vec3>,
    /// Triangles (counter-clockwise from the front).
    pub triangles: Vec<[u32; 3]>,
    /// Joint indices per vertex (empty when the mesh is not skinned).
    pub joints: Vec<[u16; 4]>,
    /// Joint weights per vertex (same length as `joints`).
    pub weights: Vec<[f32; 4]>,
    /// Texture coordinates (empty or one per vertex); used when the sim mesh is drawn.
    pub uvs: Vec<[f32; 2]>,
}

/// A render section as the cooker resolved it from glTF (model space, same space as the sim
/// mesh).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderMeshInput {
    /// Vertex positions.
    pub positions: Vec<Vec3>,
    /// Texture coordinates (empty or one per vertex).
    pub uvs: Vec<[f32; 2]>,
    /// Triangles.
    pub triangles: Vec<[u32; 3]>,
}

/// One cooked constraint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CookedConstraint {
    /// Kind (`Stretch`, `Shear` or `Bend`).
    pub kind: ConstraintKind,
    /// Particles: `a, b` for links (`c = d = 0`); hinge `a, b` and wings `c, d` for bends.
    pub ids: [u32; 4],
    /// Rest length (links) or rest dihedral angle (bends), model space.
    pub rest: f32,
    /// Compliance.
    pub compliance: f32,
}

/// The dense vertex maps of one LOD (each empty or one value per particle).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClothMaps {
    /// Reach in meters.
    pub reach: Vec<f32>,
    /// Standoff in meters.
    pub standoff: Vec<f32>,
    /// Pose pull, `0..=1`.
    pub pose_pull: Vec<f32>,
    /// Output mix, `0..=1`.
    pub sim_mix: Vec<f32>,
    /// Mass scale, `> 0`.
    pub mass_scale: Vec<f32>,
    /// Garment layer, `0..=255` (rounded; higher is farther out). Self-collision across layers is
    /// one-sided: inner layers push outer layers (`ClothSolver::set_layers`, ADR-2425).
    pub layer: Vec<f32>,
}

impl ClothMaps {
    /// The map named `name` ([`MAP_NAMES`]).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Vec<f32>> {
        Some(match name {
            "reach" => &self.reach,
            "standoff" => &self.standoff,
            "pose_pull" => &self.pose_pull,
            "sim_mix" => &self.sim_mix,
            "mass_scale" => &self.mass_scale,
            "layer" => &self.layer,
            _ => return None,
        })
    }

    /// The map named `name`, mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Vec<f32>> {
        Some(match name {
            "reach" => &mut self.reach,
            "standoff" => &mut self.standoff,
            "pose_pull" => &mut self.pose_pull,
            "sim_mix" => &mut self.sim_mix,
            "mass_scale" => &mut self.mass_scale,
            "layer" => &mut self.layer,
            _ => return None,
        })
    }

    fn all(&self) -> [&Vec<f32>; 6] {
        [
            &self.reach,
            &self.standoff,
            &self.pose_pull,
            &self.sim_mix,
            &self.mass_scale,
            &self.layer,
        ]
    }

    fn all_mut(&mut self) -> [&mut Vec<f32>; 6] {
        [
            &mut self.reach,
            &mut self.standoff,
            &mut self.pose_pull,
            &mut self.sim_mix,
            &mut self.mass_scale,
            &mut self.layer,
        ]
    }

    /// The default value of map `name` for a vertex the map does not cover.
    #[must_use]
    pub fn default_of(name: &str) -> f32 {
        match name {
            "reach" => f32::INFINITY,
            "sim_mix" | "mass_scale" => 1.0,
            _ => 0.0,
        }
    }

    /// The solver's pose maps (the first five maps).
    #[must_use]
    pub fn pose_maps(&self) -> PoseMaps {
        PoseMaps {
            reach: self.reach.clone(),
            standoff: self.standoff.clone(),
            pose_pull: self.pose_pull.clone(),
            sim_mix: self.sim_mix.clone(),
            mass_scale: self.mass_scale.clone(),
        }
    }

    /// Checks lengths and ranges (the solver's pose-map rules, `layer` in `0..=255`).
    ///
    /// # Errors
    /// As [`PoseMaps::validate`]; [`ClothError::BadParam`] for a bad layer.
    pub fn validate(&self, particles: usize) -> Result<(), ClothError> {
        self.pose_maps().validate(particles)?;
        if !self.layer.is_empty() && self.layer.len() != particles {
            return Err(ClothError::MapSizeMismatch {
                map: "layer",
                len: self.layer.len(),
                expected: particles,
            });
        }
        if self
            .layer
            .iter()
            .any(|&v| !(0.0..=255.0).contains(&v) || v.fract() != 0.0)
        {
            return Err(ClothError::BadParam { what: "layer" });
        }
        Ok(())
    }

    /// Whether any of the five pose maps holds values.
    #[must_use]
    pub fn has_pose_maps(&self) -> bool {
        self.all()[..5].iter().any(|m| !m.is_empty())
    }
}

/// Four quantized joint influences of one particle (weights sum to 255 when any is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CookedInfluence {
    /// Joint indices.
    pub joints: [u8; 4],
    /// Weights out of 255.
    pub weights: [u8; 4],
}

/// One render vertex bound to a sim triangle: the barycentric point `(1 - u - v, u, v)` of
/// the triangle plus `d` along its interpolated normal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderBinding {
    /// Sim triangle index.
    pub triangle: u32,
    /// Barycentric weight of the triangle's second vertex.
    pub u: f32,
    /// Barycentric weight of the triangle's third vertex.
    pub v: f32,
    /// Offset along the interpolated normal in meters.
    pub d: f32,
    /// Texture coordinate of the render vertex.
    pub uv: [f32; 2],
}

/// The visible section of a cloth: bound render vertices and their triangles.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderSection {
    /// One binding per render vertex.
    pub vertices: Vec<RenderBinding>,
    /// Triangles over `vertices`.
    pub indices: Vec<[u32; 3]>,
    /// True when the section comes from a separate render mesh (header flag bit 1); false when
    /// it is the sim mesh itself.
    pub from_render_mesh: bool,
}

/// One cooked LOD of a cloth asset.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClothLod {
    /// Rest positions (model space).
    pub rest_positions: Vec<Vec3>,
    /// Sim triangles.
    pub triangles: Vec<[u32; 3]>,
    /// Constraints in cooked order.
    pub constraints: Vec<CookedConstraint>,
    /// Vertex maps.
    pub maps: ClothMaps,
    /// Joint influences (empty when not skinned).
    pub skin: Vec<CookedInfluence>,
    /// The render section.
    pub render: RenderSection,
    /// Parameters of this LOD (LOD 0's with the LOD's overrides).
    pub params: ClothParams,
    /// Whether the asset turns on tethers.
    pub tethers: bool,
}

/// A cooked cloth asset (spec 55 § 4.3): LOD 0 and any coarser sim-mesh LODs.
#[derive(Debug, Clone, PartialEq)]
pub struct ClothAsset {
    /// Asset name.
    pub name: String,
    /// Body proxy names the cloth collides with (empty: every proxy under its skinned root).
    pub proxies: Vec<String>,
    /// LOD 0 first. Never empty.
    pub lods: Vec<ClothLod>,
    /// Set by map edits ([`super::maps::apply_map_edit`]); not stored.
    pub dirty: bool,
}

// ---------------------------------------------------------------------------
// Cooking
// ---------------------------------------------------------------------------

fn check_mesh(mesh: &SimMeshInput, what: &str) -> Result<(), ClothError> {
    let n = mesh.positions.len();
    if n == 0 {
        return Err(invalid(format!("{what}: no vertices")));
    }
    if n > MAX_ASSET_PARTICLES {
        return Err(invalid(format!(
            "{what}: {n} vertices (at most {MAX_ASSET_PARTICLES})"
        )));
    }
    if mesh.positions.iter().any(|p| !p.is_finite()) {
        return Err(ClothError::NonFinite { what: "position" });
    }
    for t in &mesh.triangles {
        if t.iter().any(|&i| i as usize >= n) {
            return Err(invalid(format!("{what}: triangle index out of range")));
        }
        if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
            return Err(invalid(format!("{what}: triangle repeats a vertex")));
        }
    }
    if !mesh.joints.is_empty() && (mesh.joints.len() != n || mesh.weights.len() != n) {
        return Err(invalid(format!(
            "{what}: joints/weights do not cover every vertex"
        )));
    }
    if !mesh.uvs.is_empty() && mesh.uvs.len() != n {
        return Err(invalid(format!("{what}: uvs do not cover every vertex")));
    }
    Ok(())
}

/// Edge `(min, max)` → the vertices opposite it, in triangle order.
fn edge_wings(triangles: &[[u32; 3]]) -> BTreeMap<(u32, u32), Vec<u32>> {
    let mut edges: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    for t in triangles {
        for k in 0..3 {
            let (a, b, c) = (t[k], t[(k + 1) % 3], t[(k + 2) % 3]);
            edges.entry((a.min(b), a.max(b))).or_default().push(c);
        }
    }
    edges
}

fn scale_of(scale: &[f32], ids: &[u32]) -> f32 {
    if scale.is_empty() {
        return 1.0;
    }
    ids.iter().map(|&i| scale[i as usize]).sum::<f32>() / ids.len() as f32
}

fn build_constraints(
    positions: &[Vec3],
    triangles: &[[u32; 3]],
    sets: &[ConstraintSet],
) -> Result<(Vec<CookedConstraint>, bool), ClothError> {
    let n = positions.len();
    let wings = edge_wings(triangles);
    let mut out = Vec::new();
    let mut tethers = false;
    let mut sorted: Vec<&ConstraintSet> = sets.iter().collect();
    sorted.sort_by_key(|s| s.kind);
    for set in sorted {
        if !set.compliance.is_finite() {
            return Err(ClothError::NonFinite { what: "compliance" });
        }
        if set.compliance < 0.0 {
            return Err(ClothError::Negative { what: "compliance" });
        }
        let scale = match &set.scale {
            Some(map) => {
                let values = map.expand(n, "constraint scale")?;
                if values.iter().any(|&v| v < 0.0) {
                    return Err(ClothError::BadParam {
                        what: "constraint scale",
                    });
                }
                values
            }
            None => Vec::new(),
        };
        let link = |a: u32, b: u32, kind: ConstraintKind| CookedConstraint {
            kind,
            ids: [a, b, 0, 0],
            rest: positions[a as usize].distance(positions[b as usize]),
            compliance: set.compliance * scale_of(&scale, &[a, b]),
        };
        match set.kind {
            ConstraintKind::Stretch => {
                for &(a, b) in wings.keys() {
                    out.push(link(a, b, ConstraintKind::Stretch));
                }
            }
            ConstraintKind::Shear => {
                let mut pairs: Vec<(u32, u32)> = wings
                    .values()
                    .filter(|w| w.len() == 2 && w[0] != w[1])
                    .map(|w| (w[0].min(w[1]), w[0].max(w[1])))
                    .collect();
                pairs.sort_unstable();
                pairs.dedup();
                for (a, b) in pairs {
                    out.push(link(a, b, ConstraintKind::Shear));
                }
            }
            ConstraintKind::Bend => {
                for (&(a, b), w) in &wings {
                    if w.len() != 2 || w[0] == w[1] {
                        continue;
                    }
                    let ids = [a, b, w[0], w[1]];
                    out.push(CookedConstraint {
                        kind: ConstraintKind::Bend,
                        ids,
                        rest: rest_dihedral(ids.map(|i| positions[i as usize])),
                        compliance: set.compliance * scale_of(&scale, &ids),
                    });
                }
            }
            ConstraintKind::Tether => tethers = true,
            ConstraintKind::Volume => {
                return Err(invalid("Volume constraint sets belong in a .soft.ron"));
            }
        }
    }
    Ok((out, tethers))
}

fn rest_dihedral(p: [Vec3; 4]) -> f32 {
    let e = p[1] - p[0];
    let n1 = e.cross(p[2] - p[0]);
    let n2 = (p[3] - p[0]).cross(e);
    let (l1, l2) = (n1.length(), n2.length());
    if l1 <= f32::EPSILON || l2 <= f32::EPSILON {
        return 0.0;
    }
    let (n1, n2) = (n1 / l1, n2 / l2);
    // The deterministic kernel, not `atan2`: the cooked rest angle must be bit-identical on
    // every cook host or a cloth sitting on its rest pose starts with a nonzero residual on
    // the hosts whose libm rounds the other way (ADR-4773).
    det_atan2(n1.cross(n2).length(), n1.dot(n2).clamp(-1.0, 1.0))
}

fn default_sets(params: &ClothParams) -> Vec<ConstraintSet> {
    [
        ("stretch", ConstraintKind::Stretch, params.stretch_compliance),
        ("shear", ConstraintKind::Shear, params.shear_compliance),
        ("bend", ConstraintKind::Bend, params.bend_compliance),
    ]
    .into_iter()
    .map(|(name, kind, compliance)| ConstraintSet {
        name: name.to_string(),
        kind,
        compliance,
        scale: None,
    })
    .collect()
}

/// Quantizes up to four weights to `u8` out of 255 (rounded; the largest absorbs the rest).
fn quantize_influence(joints: [u16; 4], weights: [f32; 4]) -> Result<CookedInfluence, ClothError> {
    if weights.iter().any(|w| !w.is_finite() || *w < 0.0) {
        return Err(invalid("joint weight is negative or not finite"));
    }
    let total: f32 = weights.iter().sum();
    let mut out = CookedInfluence::default();
    if total <= 0.0 {
        return Ok(out);
    }
    for (((slot_joint, slot_weight), &joint), &weight) in out
        .joints
        .iter_mut()
        .zip(out.weights.iter_mut())
        .zip(&joints)
        .zip(&weights)
    {
        if weight > 0.0 {
            *slot_joint = u8::try_from(joint).map_err(|_| invalid(format!("joint {joint} over 255")))?;
        }
        *slot_weight = (weight / total * 255.0).round() as u8;
    }
    let sum: i32 = out.weights.iter().map(|&w| i32::from(w)).sum();
    let largest = (0..4)
        .max_by_key(|&k| (out.weights[k], std::cmp::Reverse(k)))
        .unwrap_or(0);
    out.weights[largest] = (i32::from(out.weights[largest]) + 255 - sum).clamp(0, 255) as u8;
    Ok(out)
}

/// Closest point on triangle `abc` to `p` as barycentric `(u, v)` (weights of `b` and `c`).
pub(crate) fn closest_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> (f32, f32) {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return (0.0, 0.0);
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return (1.0, 0.0);
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let t = d1 / (d1 - d3);
        return (t, 0.0);
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return (0.0, 1.0);
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let t = d2 / (d2 - d6);
        return (0.0, t);
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let t = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return (1.0 - t, t);
    }
    let denom = 1.0 / (va + vb + vc);
    (vb * denom, vc * denom)
}

/// Area-weighted vertex normals of a triangle mesh (zero for a vertex no triangle touches).
pub fn vertex_normals(positions: &[Vec3], triangles: &[[u32; 3]], out: &mut Vec<Vec3>) {
    out.clear();
    out.resize(positions.len(), Vec3::ZERO);
    for t in triangles {
        let [a, b, c] = t.map(|i| positions[i as usize]);
        let n = (b - a).cross(c - a);
        for &i in t {
            out[i as usize] += n;
        }
    }
    for n in out.iter_mut() {
        *n = n.normalize_or_zero();
    }
}

/// Evaluates a binding against sim positions and vertex normals.
#[must_use]
pub fn bind_point(
    binding: &RenderBinding,
    triangles: &[[u32; 3]],
    positions: &[Vec3],
    normals: &[Vec3],
) -> (Vec3, Vec3) {
    let Some(t) = triangles.get(binding.triangle as usize) else {
        return (Vec3::ZERO, Vec3::Y);
    };
    let w = [1.0 - binding.u - binding.v, binding.u, binding.v];
    let mut p = Vec3::ZERO;
    let mut n = Vec3::ZERO;
    for (&i, &wk) in t.iter().zip(&w) {
        let i = i as usize;
        p += positions.get(i).copied().unwrap_or(Vec3::ZERO) * wk;
        n += normals.get(i).copied().unwrap_or(Vec3::ZERO) * wk;
    }
    let n = n.normalize_or_zero();
    (p + n * binding.d, n)
}

fn bind_render(sim: &SimMeshInput, render: Option<&RenderMeshInput>) -> Result<RenderSection, ClothError> {
    let Some(render) = render else {
        // The sim mesh is drawn: each vertex binds to a corner of the first triangle using it.
        let mut first: Vec<Option<(u32, usize)>> = vec![None; sim.positions.len()];
        for (ti, t) in sim.triangles.iter().enumerate() {
            for (corner, &i) in t.iter().enumerate() {
                first[i as usize].get_or_insert((ti as u32, corner));
            }
        }
        let vertices = first
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let (triangle, corner) = f.unwrap_or((0, 0));
                RenderBinding {
                    triangle,
                    u: f32::from(u8::from(corner == 1)),
                    v: f32::from(u8::from(corner == 2)),
                    d: 0.0,
                    uv: sim.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                }
            })
            .collect();
        return Ok(RenderSection {
            vertices,
            indices: sim.triangles.clone(),
            from_render_mesh: false,
        });
    };
    if sim.triangles.is_empty() {
        return Err(invalid("a render mesh needs a sim mesh with triangles"));
    }
    let n = render.positions.len();
    if render.positions.iter().any(|p| !p.is_finite()) {
        return Err(ClothError::NonFinite {
            what: "render position",
        });
    }
    if !render.uvs.is_empty() && render.uvs.len() != n {
        return Err(invalid("render mesh: uvs do not cover every vertex"));
    }
    if render.triangles.iter().flatten().any(|&i| i as usize >= n) {
        return Err(invalid("render mesh: triangle index out of range"));
    }
    let mut normals = Vec::new();
    vertex_normals(&sim.positions, &sim.triangles, &mut normals);
    let mut vertices = Vec::with_capacity(n);
    for (ri, &p) in render.positions.iter().enumerate() {
        let mut best = (f32::INFINITY, 0u32, 0.0f32, 0.0f32);
        for (ti, t) in sim.triangles.iter().enumerate() {
            let [a, b, c] = t.map(|i| sim.positions[i as usize]);
            let (u, v) = closest_on_triangle(p, a, b, c);
            let q = a * (1.0 - u - v) + b * u + c * v;
            let dist = q.distance_squared(p);
            if dist < best.0 {
                best = (dist, ti as u32, u, v);
            }
        }
        let (_, triangle, u, v) = best;
        let mut binding = RenderBinding {
            triangle,
            u,
            v,
            d: 0.0,
            uv: render.uvs.get(ri).copied().unwrap_or([0.0, 0.0]),
        };
        let (q, normal) = bind_point(&binding, &sim.triangles, &sim.positions, &normals);
        binding.d = (p - q).dot(normal);
        vertices.push(binding);
    }
    Ok(RenderSection {
        vertices,
        indices: render.triangles.clone(),
        from_render_mesh: true,
    })
}

/// LOD 0 maps resampled onto `to` by nearest LOD 0 rest vertex (ties: lower index).
fn resample_maps(from: &ClothMaps, from_rest: &[Vec3], to: &[Vec3]) -> ClothMaps {
    let nearest: Vec<usize> = to.iter().map(|&p| nearest_vertex(from_rest, p)).collect();
    let mut out = ClothMaps::default();
    for (src, dst) in from.all().into_iter().zip(out.all_mut()) {
        if !src.is_empty() {
            *dst = nearest.iter().map(|&i| src[i]).collect();
        }
    }
    out
}

/// Index of the vertex of `rest` nearest to `p` (ties: lower index).
pub(crate) fn nearest_vertex(rest: &[Vec3], p: Vec3) -> usize {
    let mut best = (f32::INFINITY, 0usize);
    for (i, &q) in rest.iter().enumerate() {
        let d = q.distance_squared(p);
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1
}

impl ClothAsset {
    /// Cooks a source with its resolved meshes: `lods[0]` is the source's sim mesh (and render
    /// section), `lods[k]` the mesh of `source.lods[k - 1]` (`None`: LOD 0's mesh).
    ///
    /// # Errors
    /// [`ClothError::AssetInvalid`] for a bad mesh, too many particles, a `Volume` set or an
    /// out-of-range joint; [`ClothError::MapSizeMismatch`] and the parameter and map range
    /// errors otherwise. Nothing is produced on error.
    pub fn cook(
        source: &ClothSource,
        lod0: &SimMeshInput,
        render: Option<&RenderMeshInput>,
        lods: &[Option<SimMeshInput>],
    ) -> Result<Self, ClothError> {
        if source.format_version == 0 || source.format_version > CLOTH_FORMAT_VERSION {
            return Err(ClothError::UnsupportedVersion {
                found: source.format_version,
                supported: CLOTH_FORMAT_VERSION,
            });
        }
        let resolved = source.with_preset_resolved()?;
        let source = &resolved;
        if lods.len() != source.lods.len() {
            return Err(invalid(format!(
                "{} LOD meshes for {} LODs",
                lods.len(),
                source.lods.len()
            )));
        }
        source.params.validate()?;
        check_mesh(lod0, "sim mesh")?;
        let n = lod0.positions.len();
        let mut maps = ClothMaps::default();
        for (name, data) in &source.maps {
            let Some(index) = MAP_NAMES.iter().position(|m| m == name) else {
                return Err(invalid(format!("unknown map {name:?}")));
            };
            *maps.all_mut()[index] = data.expand(n, MAP_NAMES[index])?;
        }
        maps.validate(n)?;
        let sets = if source.constraint_sets.is_empty() {
            default_sets(&source.params)
        } else {
            source.constraint_sets.clone()
        };
        let mut out_lods = Vec::with_capacity(1 + lods.len());
        let skin = |mesh: &SimMeshInput| -> Result<Vec<CookedInfluence>, ClothError> {
            mesh.joints
                .iter()
                .zip(&mesh.weights)
                .map(|(&j, &w)| quantize_influence(j, w))
                .collect()
        };
        let (constraints, tethers) = build_constraints(&lod0.positions, &lod0.triangles, &sets)?;
        out_lods.push(ClothLod {
            rest_positions: lod0.positions.clone(),
            triangles: lod0.triangles.clone(),
            constraints,
            maps: maps.clone(),
            skin: skin(lod0)?,
            render: bind_render(lod0, render)?,
            params: source.params,
            tethers,
        });
        for (k, (lod_source, mesh)) in source.lods.iter().zip(lods).enumerate() {
            let params = source.params.apply(&lod_source.overrides);
            params.validate()?;
            let lod = match mesh {
                Some(mesh) => {
                    check_mesh(mesh, &format!("LOD {} sim mesh", k + 1))?;
                    let sets = if source.constraint_sets.is_empty() {
                        default_sets(&params)
                    } else {
                        sets.clone()
                    };
                    let (constraints, tethers) = build_constraints(&mesh.positions, &mesh.triangles, &sets)?;
                    ClothLod {
                        rest_positions: mesh.positions.clone(),
                        triangles: mesh.triangles.clone(),
                        constraints,
                        maps: resample_maps(&maps, &lod0.positions, &mesh.positions),
                        skin: skin(mesh)?,
                        render: bind_render(mesh, render)?,
                        params,
                        tethers,
                    }
                }
                None => ClothLod {
                    params,
                    ..out_lods[0].clone()
                },
            };
            out_lods.push(lod);
        }
        Ok(Self {
            name: source.name.clone(),
            proxies: source.proxies.clone(),
            lods: out_lods,
            dirty: false,
        })
    }

    /// LOD 0.
    #[must_use]
    pub fn lod0(&self) -> &ClothLod {
        &self.lods[0]
    }

    /// How a solver of LOD `to` continues one of LOD `from`: per `to` vertex, the nearest `from`
    /// rest vertex (ties: lower index) and the rest offset from it (spec 55 § 2.9, for
    /// [`ClothSolver::adopt_state`]). Empty when either LOD does not exist.
    #[must_use]
    pub fn lod_transfer(&self, from: usize, to: usize) -> Vec<(u32, Vec3)> {
        let (Some(from), Some(to)) = (self.lods.get(from), self.lods.get(to)) else {
            return Vec::new();
        };
        if from.rest_positions.is_empty() {
            return Vec::new();
        }
        to.rest_positions
            .iter()
            .map(|&p| {
                let nearest = nearest_vertex(&from.rest_positions, p);
                (nearest as u32, p - from.rest_positions[nearest])
            })
            .collect()
    }

    /// Whether the asset carries joint influences (a skinned cloth).
    #[must_use]
    pub fn is_skinned(&self) -> bool {
        self.lods.first().is_some_and(|l| !l.skin.is_empty())
    }

    /// Builds a solver for LOD `lod` (clamped to the last LOD) at fixed step `time_step`, with
    /// the rest pose placed by `world`. Pose maps are set when the asset has any; the solver
    /// gets one triangle per sim triangle (wind, normals, tearing).
    ///
    /// # Errors
    /// The solver's errors (a degenerate world transform, bad parameters).
    pub fn build_solver(
        &self,
        lod: usize,
        time_step: f32,
        world: Affine3A,
    ) -> Result<ClothSolver, ClothError> {
        let Some(last) = self.lods.len().checked_sub(1) else {
            return Err(invalid("asset has no LOD"));
        };
        let params = self.lods[lod.min(last)].params;
        self.build_solver_with(
            lod,
            time_step,
            world,
            params.for_platform(cfg!(target_os = "android")),
        )
    }

    /// As [`ClothAsset::build_solver`] with `params` used as given instead of LOD `lod`'s own (a
    /// game's `ClothSim.params`). The stretch, shear and bend compliances of `params` are not
    /// applied: the constraints keep the compliance baked at cook time (ADR-2250).
    ///
    /// # Errors
    /// As [`ClothAsset::build_solver`].
    pub fn build_solver_with(
        &self,
        lod: usize,
        time_step: f32,
        world: Affine3A,
        params: ClothParams,
    ) -> Result<ClothSolver, ClothError> {
        let Some(last) = self.lods.len().checked_sub(1) else {
            return Err(invalid("asset has no LOD"));
        };
        let lod = &self.lods[lod.min(last)];
        if lod
            .triangles
            .iter()
            .any(|t| t.iter().any(|&i| i as usize >= lod.rest_positions.len()))
        {
            return Err(invalid("triangle index out of range"));
        }
        let mut solver = ClothSolver::new(params.config(time_step))?;
        let mut tuning = params.tuning();
        if lod.tethers && tuning.tether.is_none() {
            tuning.tether = Some(1.0);
        }
        let mut area = vec![0.0f32; lod.rest_positions.len()];
        for t in &lod.triangles {
            let [a, b, c] = t.map(|i| lod.rest_positions[i as usize]);
            let third = (b - a).cross(c - a).length() / 6.0;
            for &i in t {
                area[i as usize] += third;
            }
        }
        for (p, a) in lod.rest_positions.iter().zip(&area) {
            let mass = if *a > 0.0 {
                a * params.density
            } else {
                LONE_PARTICLE_MASS
            };
            solver.add_particle(world.transform_point3(*p), 1.0 / mass.max(1e-9))?;
        }
        for c in &lod.constraints {
            let [a, b, c2, d] = c.ids.map(|i| i as usize);
            match c.kind {
                ConstraintKind::Bend => {
                    solver.add_bend(a, b, c2, d, c.compliance)?;
                }
                _ => {
                    solver.add_distance(a, b, c.compliance)?;
                }
            }
        }
        for t in &lod.triangles {
            solver.add_triangle(t[0] as usize, t[1] as usize, t[2] as usize)?;
        }
        if lod.maps.has_pose_maps() {
            solver.set_pose_maps(lod.maps.pose_maps())?;
        }
        if !lod.maps.layer.is_empty() {
            // Validated to `0..=255` on load; `as` saturates anything else.
            let layers: Vec<u8> = lod.maps.layer.iter().map(|l| l.round() as u8).collect();
            solver.set_layers(&layers)?;
        }
        solver.set_tuning(tuning)?;
        Ok(solver)
    }
}

// ---------------------------------------------------------------------------
// Binary payload
// ---------------------------------------------------------------------------

struct Writer(Vec<u8>);

impl Writer {
    fn u8s(&mut self, v: &[u8]) {
        self.0.extend_from_slice(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&mut self, v: usize) {
        self.u32(u32::try_from(v).unwrap_or(u32::MAX));
    }
    /// A length-prefixed string padded to 4 bytes.
    fn str(&mut self, s: &str) {
        self.len(s.len());
        self.0.extend_from_slice(s.as_bytes());
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
    }
    fn f32s(&mut self, v: &[f32]) {
        self.len(v.len());
        for &x in v {
            self.f32(x);
        }
    }
    fn vec3s(&mut self, v: &[Vec3]) {
        for p in v {
            self.f32(p.x);
            self.f32(p.y);
            self.f32(p.z);
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

fn truncated() -> ClothError {
    invalid("cooked payload is truncated")
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ClothError> {
        let end = self.at.checked_add(n).ok_or_else(truncated)?;
        let out = self.bytes.get(self.at..end).ok_or_else(truncated)?;
        self.at = end;
        Ok(out)
    }
    fn u32(&mut self) -> Result<u32, ClothError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn f32(&mut self) -> Result<f32, ClothError> {
        Ok(f32::from_bits(self.u32()?))
    }
    /// A count, checked against the bytes left (each item at least `item` bytes).
    fn count(&mut self, item: usize) -> Result<usize, ClothError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(item) > self.bytes.len() - self.at {
            return Err(truncated());
        }
        Ok(n)
    }
    fn str(&mut self) -> Result<String, ClothError> {
        let n = self.count(1)?;
        let s = std::str::from_utf8(self.take(n)?)
            .map_err(|_| invalid("cooked string is not UTF-8"))?
            .to_string();
        let pad = (4 - n % 4) % 4;
        self.take(pad)?;
        Ok(s)
    }
    fn f32s(&mut self) -> Result<Vec<f32>, ClothError> {
        let n = self.count(4)?;
        (0..n).map(|_| self.f32()).collect()
    }
    fn vec3s(&mut self, n: usize) -> Result<Vec<Vec3>, ClothError> {
        if n.saturating_mul(12) > self.bytes.len() - self.at {
            return Err(truncated());
        }
        (0..n)
            .map(|_| Ok(Vec3::new(self.f32()?, self.f32()?, self.f32()?)))
            .collect()
    }
    fn index(&mut self, len: usize) -> Result<u32, ClothError> {
        let i = self.u32()?;
        if i as usize >= len {
            return Err(invalid("cooked index out of range"));
        }
        Ok(i)
    }
    fn magic(&mut self, magic: [u8; 4], version: u32) -> Result<(), ClothError> {
        if self.take(4)? != magic {
            return Err(invalid("not a cooked cloth or soft payload"));
        }
        let found = self.u32()?;
        if found > version {
            return Err(ClothError::UnsupportedVersion {
                found,
                supported: version,
            });
        }
        if found == 0 {
            return Err(invalid("payload version 0"));
        }
        Ok(())
    }
    fn end(&self) -> Result<(), ClothError> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("trailing bytes after the cooked payload"))
        }
    }
}

const FLAG_POSE_MAPS: u32 = 1;
const FLAG_RENDER_BINDING: u32 = 2;
const FLAG_TEARABLE: u32 = 4;
const FLAG_TETHERS: u32 = 8;

fn write_lod(w: &mut Writer, lod: &ClothLod) -> Result<(), ClothError> {
    let mut flags = 0;
    if lod.maps.has_pose_maps() {
        flags |= FLAG_POSE_MAPS;
    }
    if lod.render.from_render_mesh {
        flags |= FLAG_RENDER_BINDING;
    }
    if lod.params.tear.is_some() {
        flags |= FLAG_TEARABLE;
    }
    if lod.tethers {
        flags |= FLAG_TETHERS;
    }
    w.len(lod.rest_positions.len());
    w.len(lod.triangles.len());
    w.len(lod.constraints.len());
    w.u32(flags);
    w.vec3s(&lod.rest_positions);
    for t in &lod.triangles {
        t.iter().for_each(|&i| w.u32(i));
    }
    for c in &lod.constraints {
        w.u8s(&[c.kind.code(), 0, 0, 0]);
        c.ids.iter().for_each(|&i| w.u32(i));
        w.f32(c.rest);
        w.f32(c.compliance);
    }
    for map in lod.maps.all() {
        w.f32s(map);
    }
    w.len(lod.skin.len());
    for s in &lod.skin {
        w.u8s(&s.joints);
        w.u8s(&s.weights);
    }
    w.len(lod.render.vertices.len());
    for b in &lod.render.vertices {
        w.u32(b.triangle);
        w.f32(b.u);
        w.f32(b.v);
        w.f32(b.d);
        w.f32(b.uv[0]);
        w.f32(b.uv[1]);
    }
    w.len(lod.render.indices.len());
    for t in &lod.render.indices {
        t.iter().for_each(|&i| w.u32(i));
    }
    let params = ron::to_string(&lod.params).map_err(|e| invalid(e.to_string()))?;
    w.str(&params);
    Ok(())
}

/// Reads a skin block: empty, or one influence per particle.
fn read_skin(r: &mut Reader<'_>, particles: usize) -> Result<Vec<CookedInfluence>, ClothError> {
    let skins = r.count(8)?;
    let mut skin = Vec::with_capacity(skins);
    for _ in 0..skins {
        let b = r.take(8)?;
        skin.push(CookedInfluence {
            joints: [b[0], b[1], b[2], b[3]],
            weights: [b[4], b[5], b[6], b[7]],
        });
    }
    if !skin.is_empty() && skin.len() != particles {
        return Err(invalid("cooked skin does not cover every particle"));
    }
    Ok(skin)
}

fn read_lod(r: &mut Reader<'_>) -> Result<ClothLod, ClothError> {
    let particles = r.u32()? as usize;
    let triangles = r.u32()? as usize;
    let constraints = r.u32()? as usize;
    let flags = r.u32()?;
    if particles == 0 || particles > MAX_ASSET_PARTICLES {
        return Err(invalid("cooked particle count out of range"));
    }
    let rest_positions = r.vec3s(particles)?;
    if triangles.saturating_mul(12) > r.bytes.len() - r.at {
        return Err(truncated());
    }
    let mut tris = Vec::with_capacity(triangles);
    for _ in 0..triangles {
        tris.push([r.index(particles)?, r.index(particles)?, r.index(particles)?]);
    }
    if constraints.saturating_mul(28) > r.bytes.len() - r.at {
        return Err(truncated());
    }
    let mut cooked = Vec::with_capacity(constraints);
    for _ in 0..constraints {
        let head = r.take(4)?;
        let kind = ConstraintKind::from_code(head[0]).ok_or_else(|| invalid("unknown constraint kind"))?;
        let ids = [
            r.index(particles)?,
            r.index(particles)?,
            r.index(particles)?,
            r.index(particles)?,
        ];
        cooked.push(CookedConstraint {
            kind,
            ids,
            rest: r.f32()?,
            compliance: r.f32()?,
        });
    }
    let mut maps = ClothMaps::default();
    for slot in maps.all_mut() {
        *slot = r.f32s()?;
    }
    maps.validate(particles)?;
    let skin = read_skin(r, particles)?;
    let bound = r.count(24)?;
    let mut vertices = Vec::with_capacity(bound);
    for _ in 0..bound {
        let triangle = r.u32()?;
        if triangle as usize >= triangles.max(1) {
            return Err(invalid("cooked binding names a missing triangle"));
        }
        vertices.push(RenderBinding {
            triangle,
            u: r.f32()?,
            v: r.f32()?,
            d: r.f32()?,
            uv: [r.f32()?, r.f32()?],
        });
    }
    let render_tris = r.count(12)?;
    let mut indices = Vec::with_capacity(render_tris);
    for _ in 0..render_tris {
        indices.push([r.index(bound)?, r.index(bound)?, r.index(bound)?]);
    }
    let params: ClothParams = ron::from_str(&r.str()?).map_err(|e| invalid(e.to_string()))?;
    params.validate()?;
    Ok(ClothLod {
        rest_positions,
        triangles: tris,
        constraints: cooked,
        maps,
        skin,
        render: RenderSection {
            vertices,
            indices,
            from_render_mesh: flags & FLAG_RENDER_BINDING != 0,
        },
        params,
        tethers: flags & FLAG_TETHERS != 0,
    })
}

impl ClothAsset {
    /// The cooked payload (spec 55 § 4.3; the caller wraps it in the `FCKD` header).
    ///
    /// # Errors
    /// [`ClothError::AssetInvalid`] when the parameters cannot be written.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ClothError> {
        let mut w = Writer(Vec::new());
        w.u8s(&CLOTH_PAYLOAD_MAGIC);
        w.u32(COOKED_CLOTH_FORMAT_VERSION);
        w.str(&self.name);
        w.len(self.proxies.len());
        for p in &self.proxies {
            w.str(p);
        }
        w.len(self.lods.len());
        for lod in &self.lods {
            write_lod(&mut w, lod)?;
        }
        Ok(w.0)
    }

    /// Reads a cooked payload, checking every index, map and parameter.
    ///
    /// # Errors
    /// [`ClothError::UnsupportedVersion`] for a newer payload; [`ClothError::AssetInvalid`] for
    /// a bad magic, truncation, an index out of range or trailing bytes; the map and parameter
    /// range errors.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ClothError> {
        let mut r = Reader { bytes, at: 0 };
        r.magic(CLOTH_PAYLOAD_MAGIC, COOKED_CLOTH_FORMAT_VERSION)?;
        let name = r.str()?;
        let proxy_count = r.count(4)?;
        let proxies = (0..proxy_count).map(|_| r.str()).collect::<Result<Vec<_>, _>>()?;
        let lod_count = r.count(16)?;
        if lod_count == 0 {
            return Err(invalid("cooked cloth has no LOD"));
        }
        let lods = (0..lod_count)
            .map(|_| read_lod(&mut r))
            .collect::<Result<Vec<_>, _>>()?;
        r.end()?;
        Ok(Self {
            name,
            proxies,
            lods,
            dirty: false,
        })
    }

    /// Reads a whole cooked `.cloth.bin` file (the `FCKD` header and the payload), as the
    /// runtime gets it from `AssetServer::read_cooked`.
    ///
    /// # Errors
    /// [`ClothError::AssetInvalid`] for a bad header or another cooked type; else as
    /// [`ClothAsset::from_bytes`].
    pub fn from_cooked_file(bytes: &[u8]) -> Result<Self, ClothError> {
        Self::from_bytes(cooked_payload(
            bytes,
            COOKED_CLOTH_TYPE_PATH,
            "not a cooked cloth",
        )?)
    }
}

// ---------------------------------------------------------------------------
// Soft assets
// ---------------------------------------------------------------------------

/// The cooked model of a soft asset.
#[derive(Debug, Clone, PartialEq)]
pub enum SoftModelData {
    /// Pressure over the surface triangles.
    Pressure {
        /// Target volume ratio.
        pressure: f32,
        /// Volume compliance.
        volume_compliance: f32,
    },
    /// Tetrahedra over the particles.
    Tetra {
        /// Tetrahedra (positive rest volume).
        tets: Vec<[u32; 4]>,
        /// Volume compliance.
        volume_compliance: f32,
    },
    /// Shape-matching clusters.
    ShapeMatch {
        /// Particle indices per cluster.
        clusters: Vec<Vec<u32>>,
        /// Pull toward the goal.
        stiffness: f32,
        /// Linear blend.
        beta: f32,
    },
}

/// One render vertex of a tetra soft body bound to a tet by barycentric weights.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TetBinding {
    /// Tet index.
    pub tet: u32,
    /// Weights of the tet's four corners (sum 1).
    pub weights: [f32; 4],
}

/// A cooked soft asset (spec 55 § 2.8, § 4.3).
#[derive(Debug, Clone, PartialEq)]
pub struct SoftAsset {
    /// Asset name.
    pub name: String,
    /// Particle rest positions (model space). For `Tetra` these are the tet mesh vertices;
    /// otherwise the surface vertices.
    pub rest_positions: Vec<Vec3>,
    /// Surface triangles: over the particles (pressure, shape match) or over the render
    /// vertices (`Tetra`).
    pub surface_triangles: Vec<[u32; 3]>,
    /// The model.
    pub model: SoftModelData,
    /// `Tetra` only: one binding per surface vertex (empty otherwise).
    pub surface_binding: Vec<TetBinding>,
    /// Texture coordinates of the drawn vertices (empty or one per drawn vertex).
    pub uvs: Vec<[f32; 2]>,
    /// Maps `reach`, `pose_pull`, `mass_scale` (each empty or one value per particle).
    pub maps: ClothMaps,
    /// Joint influences per particle (empty when not skinned): flesh on a skeleton (spec 55
    /// § 2.8). Generated tet vertices take the nearest surface vertex's influences.
    pub skin: Vec<CookedInfluence>,
    /// Parameters.
    pub params: SoftParamsSource,
}

/// The tet mesh a cooker generated (or the source listed) for a `Tetra` soft body.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TetMeshInput {
    /// Tet mesh vertices.
    pub positions: Vec<Vec3>,
    /// Tetrahedra with positive volume.
    pub tets: Vec<[u32; 4]>,
    /// One binding per surface vertex.
    pub surface_binding: Vec<TetBinding>,
}

impl SoftAsset {
    /// Cooks a soft source with its resolved surface (and, for `Tetra`, its tet mesh; when the
    /// source lists `tets` they index the surface vertices and `tets_mesh` is `None`).
    ///
    /// # Errors
    /// [`ClothError::NotClosed`], [`ClothError::InvertedRest`], [`ClothError::BadCluster`] and
    /// the other soft-body errors (checked by building a solver), [`ClothError::AssetInvalid`]
    /// for a bad mesh.
    pub fn cook(
        source: &SoftSource,
        surface: &SimMeshInput,
        tets_mesh: Option<&TetMeshInput>,
    ) -> Result<Self, ClothError> {
        if source.format_version == 0 || source.format_version > SOFT_FORMAT_VERSION {
            return Err(ClothError::UnsupportedVersion {
                found: source.format_version,
                supported: SOFT_FORMAT_VERSION,
            });
        }
        check_mesh(surface, "surface")?;
        let (rest_positions, model, surface_binding) = match &source.model {
            SoftModelSource::Pressure {
                pressure,
                volume_compliance,
            } => (
                surface.positions.clone(),
                SoftModelData::Pressure {
                    pressure: *pressure,
                    volume_compliance: *volume_compliance,
                },
                Vec::new(),
            ),
            SoftModelSource::Tetra {
                tets,
                volume_compliance,
                ..
            } => match tets_mesh {
                Some(mesh) if tets.is_empty() => {
                    if mesh.surface_binding.len() != surface.positions.len() {
                        return Err(invalid("tet binding does not cover the surface"));
                    }
                    if mesh.positions.len() > MAX_ASSET_PARTICLES {
                        return Err(invalid("too many tet vertices"));
                    }
                    if mesh
                        .surface_binding
                        .iter()
                        .any(|b| b.tet as usize >= mesh.tets.len())
                    {
                        return Err(invalid("tet binding names a missing tet"));
                    }
                    (
                        mesh.positions.clone(),
                        SoftModelData::Tetra {
                            tets: mesh.tets.clone(),
                            volume_compliance: *volume_compliance,
                        },
                        mesh.surface_binding.clone(),
                    )
                }
                _ if !tets.is_empty() => (
                    surface.positions.clone(),
                    SoftModelData::Tetra {
                        tets: tets.clone(),
                        volume_compliance: *volume_compliance,
                    },
                    Vec::new(),
                ),
                _ => return Err(invalid("a Tetra model needs tets or a generated tet mesh")),
            },
            SoftModelSource::ShapeMatch {
                clusters,
                stiffness,
                beta,
            } => (
                surface.positions.clone(),
                SoftModelData::ShapeMatch {
                    clusters: if clusters.is_empty() {
                        vec![(0..surface.positions.len() as u32).collect()]
                    } else {
                        clusters.clone()
                    },
                    stiffness: *stiffness,
                    beta: *beta,
                },
                Vec::new(),
            ),
        };
        let n = rest_positions.len();
        let mut maps = ClothMaps::default();
        for (name, data) in &source.maps {
            match name.as_str() {
                "reach" => maps.reach = data.expand(n, "reach")?,
                "pose_pull" => maps.pose_pull = data.expand(n, "pose_pull")?,
                "mass_scale" => maps.mass_scale = data.expand(n, "mass_scale")?,
                other => return Err(invalid(format!("map {other:?} is not used by soft bodies"))),
            }
        }
        maps.validate(n)?;
        let skin = if surface.joints.is_empty() {
            Vec::new()
        } else {
            rest_positions
                .iter()
                .enumerate()
                .map(|(i, &p)| {
                    let at = if surface_binding.is_empty() {
                        i
                    } else {
                        nearest_vertex(&surface.positions, p)
                    };
                    quantize_influence(surface.joints[at], surface.weights[at])
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        let asset = Self {
            name: source.name.clone(),
            rest_positions,
            surface_triangles: surface.triangles.clone(),
            model,
            surface_binding,
            uvs: surface.uvs.clone(),
            maps,
            skin,
            params: source.params,
        };
        asset.build_solver(1.0 / 60.0, Affine3A::IDENTITY)?;
        Ok(asset)
    }

    /// Builds a soft solver at fixed step `time_step`, rest pose placed by `world`.
    ///
    /// A mirroring `world` (negative determinant) turns every signed volume inside out, so the
    /// solver gets the surface triangles and tets with two corners swapped: its volumes stay
    /// positive and the body keeps its shape. The asset's own index order is unchanged.
    ///
    /// # Errors
    /// The soft-body errors of [`SoftSolver::new`].
    pub fn build_solver(&self, time_step: f32, world: Affine3A) -> Result<SoftSolver, ClothError> {
        let positions: Vec<Vec3> = self
            .rest_positions
            .iter()
            .map(|&p| world.transform_point3(p))
            .collect();
        let mirrored = world.matrix3.determinant() < 0.0;
        let model = match &self.model {
            SoftModelData::Pressure {
                pressure,
                volume_compliance,
            } => SoftModel::Pressure {
                triangles: self
                    .surface_triangles
                    .iter()
                    .map(|t| {
                        let t = t.map(|i| i as usize);
                        if mirrored { [t[0], t[2], t[1]] } else { t }
                    })
                    .collect(),
                pressure: *pressure,
                volume_compliance: *volume_compliance,
            },
            SoftModelData::Tetra {
                tets,
                volume_compliance,
            } => SoftModel::Tetra {
                tets: tets
                    .iter()
                    .map(|t| {
                        let t = t.map(|i| i as usize);
                        if mirrored { [t[0], t[2], t[1], t[3]] } else { t }
                    })
                    .collect(),
                volume_compliance: *volume_compliance,
            },
            SoftModelData::ShapeMatch {
                clusters,
                stiffness,
                beta,
            } => SoftModel::ShapeMatch {
                clusters: clusters
                    .iter()
                    .map(|c| c.iter().map(|&i| i as usize).collect())
                    .collect(),
                stiffness: *stiffness,
                beta: *beta,
            },
        };
        let params = self.params.for_platform(cfg!(target_os = "android"));
        let mut solver = SoftSolver::new(&positions, model, params.params(time_step))?;
        if self.maps.has_pose_maps() {
            solver.cloth_mut().set_pose_maps(self.maps.pose_maps())?;
        }
        Ok(solver)
    }

    /// Number of drawn vertices (surface vertices).
    #[must_use]
    pub fn drawn_vertex_count(&self) -> usize {
        if self.surface_binding.is_empty() {
            self.rest_positions.len()
        } else {
            self.surface_binding.len()
        }
    }

    /// The cooked payload.
    ///
    /// # Errors
    /// [`ClothError::AssetInvalid`] when the parameters cannot be written.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ClothError> {
        let mut w = Writer(Vec::new());
        w.u8s(&SOFT_PAYLOAD_MAGIC);
        w.u32(COOKED_SOFT_FORMAT_VERSION);
        w.str(&self.name);
        w.len(self.rest_positions.len());
        w.vec3s(&self.rest_positions);
        w.len(self.surface_triangles.len());
        for t in &self.surface_triangles {
            t.iter().for_each(|&i| w.u32(i));
        }
        match &self.model {
            SoftModelData::Pressure {
                pressure,
                volume_compliance,
            } => {
                w.u8s(&[0, 0, 0, 0]);
                w.f32(*pressure);
                w.f32(*volume_compliance);
            }
            SoftModelData::Tetra {
                tets,
                volume_compliance,
            } => {
                w.u8s(&[1, 0, 0, 0]);
                w.f32(*volume_compliance);
                w.len(tets.len());
                for t in tets {
                    t.iter().for_each(|&i| w.u32(i));
                }
            }
            SoftModelData::ShapeMatch {
                clusters,
                stiffness,
                beta,
            } => {
                w.u8s(&[2, 0, 0, 0]);
                w.f32(*stiffness);
                w.f32(*beta);
                w.len(clusters.len());
                for c in clusters {
                    w.len(c.len());
                    c.iter().for_each(|&i| w.u32(i));
                }
            }
        }
        w.len(self.surface_binding.len());
        for b in &self.surface_binding {
            w.u32(b.tet);
            b.weights.iter().for_each(|&x| w.f32(x));
        }
        w.len(self.uvs.len());
        for uv in &self.uvs {
            w.f32(uv[0]);
            w.f32(uv[1]);
        }
        w.f32s(&self.maps.reach);
        w.f32s(&self.maps.pose_pull);
        w.f32s(&self.maps.mass_scale);
        w.len(self.skin.len());
        for s in &self.skin {
            w.u8s(&s.joints);
            w.u8s(&s.weights);
        }
        let params = ron::to_string(&self.params).map_err(|e| invalid(e.to_string()))?;
        w.str(&params);
        Ok(w.0)
    }

    /// Whether the asset carries joint influences (flesh on a skeleton).
    #[must_use]
    pub fn is_skinned(&self) -> bool {
        !self.skin.is_empty()
    }

    /// Whether the body attaches to a pose (a `reach` or `pose_pull` map, spec 55 § 2.8). A free
    /// body has no pose: `mass_scale` alone does not make one.
    #[must_use]
    pub fn is_posed(&self) -> bool {
        !self.maps.reach.is_empty() || !self.maps.pose_pull.is_empty()
    }

    /// Reads a whole cooked `.soft.bin` file (the `FCKD` header and the payload).
    ///
    /// # Errors
    /// As [`ClothAsset::from_cooked_file`], plus the soft-body build errors.
    pub fn from_cooked_file(bytes: &[u8]) -> Result<Self, ClothError> {
        Self::from_bytes(cooked_payload(
            bytes,
            COOKED_SOFT_TYPE_PATH,
            "not a cooked soft body",
        )?)
    }

    /// Reads a cooked payload and checks it by building a solver.
    ///
    /// # Errors
    /// As [`ClothAsset::from_bytes`], plus the soft-body build errors.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ClothError> {
        let mut r = Reader { bytes, at: 0 };
        r.magic(SOFT_PAYLOAD_MAGIC, COOKED_SOFT_FORMAT_VERSION)?;
        let name = r.str()?;
        let n = r.count(12)?;
        if n == 0 || n > MAX_ASSET_PARTICLES {
            return Err(invalid("cooked particle count out of range"));
        }
        let rest_positions = r.vec3s(n)?;
        let tri_count = r.count(12)?;
        let mut surface_triangles = Vec::with_capacity(tri_count);
        for _ in 0..tri_count {
            surface_triangles.push([r.u32()?, r.u32()?, r.u32()?]);
        }
        let kind = r.take(4)?[0];
        let model = match kind {
            0 => SoftModelData::Pressure {
                pressure: r.f32()?,
                volume_compliance: r.f32()?,
            },
            1 => {
                let volume_compliance = r.f32()?;
                let count = r.count(16)?;
                let mut tets = Vec::with_capacity(count);
                for _ in 0..count {
                    tets.push([r.index(n)?, r.index(n)?, r.index(n)?, r.index(n)?]);
                }
                SoftModelData::Tetra {
                    tets,
                    volume_compliance,
                }
            }
            2 => {
                let stiffness = r.f32()?;
                let beta = r.f32()?;
                let count = r.count(4)?;
                let mut clusters = Vec::with_capacity(count);
                for _ in 0..count {
                    let len = r.count(4)?;
                    clusters.push((0..len).map(|_| r.index(n)).collect::<Result<Vec<_>, _>>()?);
                }
                SoftModelData::ShapeMatch {
                    clusters,
                    stiffness,
                    beta,
                }
            }
            _ => return Err(invalid("unknown soft model")),
        };
        let bound = r.count(20)?;
        let mut surface_binding = Vec::with_capacity(bound);
        for _ in 0..bound {
            surface_binding.push(TetBinding {
                tet: r.u32()?,
                weights: [r.f32()?, r.f32()?, r.f32()?, r.f32()?],
            });
        }
        let uv_count = r.count(8)?;
        let mut uvs = Vec::with_capacity(uv_count);
        for _ in 0..uv_count {
            uvs.push([r.f32()?, r.f32()?]);
        }
        let maps = ClothMaps {
            reach: r.f32s()?,
            pose_pull: r.f32s()?,
            mass_scale: r.f32s()?,
            ..ClothMaps::default()
        };
        let skin = read_skin(&mut r, n)?;
        let params: SoftParamsSource = ron::from_str(&r.str()?).map_err(|e| invalid(e.to_string()))?;
        r.end()?;
        let drawn = if surface_binding.is_empty() {
            n
        } else {
            surface_binding.len()
        };
        if surface_triangles.iter().flatten().any(|&i| i as usize >= drawn) {
            return Err(invalid("cooked surface index out of range"));
        }
        if let SoftModelData::Tetra { tets, .. } = &model
            && surface_binding.iter().any(|b| b.tet as usize >= tets.len())
        {
            return Err(invalid("cooked tet binding names a missing tet"));
        }
        maps.validate(n)?;
        let asset = Self {
            name,
            rest_positions,
            surface_triangles,
            model,
            surface_binding,
            uvs,
            maps,
            skin,
            params,
        };
        asset.build_solver(1.0 / 60.0, Affine3A::IDENTITY)?;
        Ok(asset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same hinge geometry as `crate::cloth::tests::hinge(0.3)`; the cooked rest and the
    /// solver's rest must be the same bits (ADR-4773, audit G finding 1). Keep in sync with
    /// `HINGE_0_3_ANGLE_BITS` there.
    const HINGE_0_3_ANGLE_BITS: u32 = 1_057_455_288;

    #[test]
    fn cooked_bend_rest_uses_the_deterministic_kernel_bits() {
        let p = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.4, 0.1, 0.8),
            Vec3::new(0.6, 0.3, -0.7),
        ];
        assert_eq!(
            rest_dihedral(p).to_bits(),
            HINGE_0_3_ANGLE_BITS,
            "a cloth cooked on one host must rest at zero bend residual on every host"
        );
    }

    fn grid(n: u32, spacing: f32) -> SimMeshInput {
        let mut mesh = SimMeshInput::default();
        for r in 0..n {
            for c in 0..n {
                mesh.positions
                    .push(Vec3::new(c as f32 * spacing, -(r as f32) * spacing, 0.0));
                mesh.uvs
                    .push([c as f32 / (n - 1) as f32, r as f32 / (n - 1) as f32]);
            }
        }
        for r in 0..n - 1 {
            for c in 0..n - 1 {
                let i = r * n + c;
                mesh.triangles.push([i, i + n, i + 1]);
                mesh.triangles.push([i + 1, i + n, i + n + 1]);
            }
        }
        mesh
    }

    fn source() -> ClothSource {
        ClothSource::from_ron_str(
            r#"(
  format_version: 1,
  name: "flag",
  sim_mesh: (source: "flag.glb", node: "Flag_Sim"),
  params: (substeps: 4, pull_compliance: 0.001, wind_model: Aerodynamic(drag: 1.0, lift: 0.3)),
  maps: {
    "reach": Sparse(default: 1.0, entries: [(0, 0.0), (1, 0.0), (2, 0.0)]),
  },
  constraint_sets: [(name: "stretch", kind: Stretch, compliance: 0.0),
                    (name: "bend", kind: Bend, compliance: 0.01)],
  lods: [(overrides: ())],
)"#,
        )
        .expect("parses")
    }

    #[test]
    fn source_preset_round_trips_resolves_and_rejects_unknown_names() {
        let text = r#"(format_version: 1, name: "cape", sim_mesh: (source: "a", node: "b"),
            preset: Some("silk"), params: (damping: 2.5))"#;
        let s = ClothSource::from_ron_str(text).expect("parses");
        assert_eq!(s.preset.as_deref(), Some("silk"));
        let again = ClothSource::from_ron_str(&s.to_ron_string().expect("writes")).expect("round trip");
        assert_eq!(again, s);
        let resolved = s.with_preset_resolved().expect("resolves");
        assert_eq!(resolved.preset, None);
        let silk = super::super::presets::ClothPreset::Silk.row();
        assert_eq!(resolved.params.damping, 2.5, "author value wins");
        assert_eq!(resolved.params.density, silk.density, "preset fills the rest");
        // An older file without the field still reads, with no preset.
        assert_eq!(source().preset, None);
        let bad =
            r#"(format_version: 1, name: "x", sim_mesh: (source: "a", node: "b"), preset: Some("tweed"))"#;
        let err = ClothSource::from_ron_str(bad).expect_err("unknown preset");
        assert_eq!(err.code(), "cloth_asset_invalid");
        assert!(err.to_string().contains("unknown_cloth_preset"), "{err}");
    }

    #[test]
    fn source_parses_with_defaults_and_rejects_unknown_maps_and_newer_versions() {
        let s = source();
        assert_eq!(s.params.substeps, 4);
        assert_eq!(s.params.density, ClothParams::default().density);
        assert_eq!(s.lods.len(), 1);
        let unknown = r#"(format_version: 1, name: "x", sim_mesh: (source: "a", node: "b"), maps: { "stiffness": Dense([]) })"#;
        let err = ClothSource::from_ron_str(unknown).expect_err("unknown map");
        assert_eq!(err.code(), "cloth_asset_invalid");
        let newer = r#"(format_version: 2, name: "x", sim_mesh: (source: "a", node: "b"))"#;
        let err = ClothSource::from_ron_str(newer).expect_err("newer");
        assert_eq!(err.code(), "cloth_unsupported_version");
        let text = s.to_ron_string().expect("writes");
        assert_eq!(ClothSource::from_ron_str(&text).expect("round trip"), s);
    }

    #[test]
    fn cook_builds_constraints_maps_and_binding_and_round_trips() {
        let s = source();
        let mesh = grid(3, 0.1);
        let asset = ClothAsset::cook(&s, &mesh, None, &[None]).expect("cooks");
        let lod = asset.lod0();
        // 3x3 grid: 16 unique edges, 8 interior edges -> 8 bends; no shear set listed.
        let stretch = lod
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Stretch)
            .count();
        let bend = lod
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Bend)
            .count();
        assert_eq!((stretch, bend), (16, 8));
        assert!(lod.constraints.iter().all(|c| c.kind != ConstraintKind::Shear));
        assert_eq!(&lod.maps.reach[..4], &[0.0, 0.0, 0.0, 1.0]);
        assert_eq!(lod.render.vertices.len(), 9);
        assert_eq!(asset.lods.len(), 2);
        let bytes = asset.to_bytes().expect("writes");
        let back = ClothAsset::from_bytes(&bytes).expect("reads");
        assert_eq!(back, asset);
        // Cooking twice is byte-identical.
        let again = ClothAsset::cook(&s, &mesh, None, &[None])
            .expect("cooks")
            .to_bytes()
            .expect("writes");
        assert_eq!(again, bytes);
        // A newer payload version is refused, truncation is refused.
        let mut newer = bytes.clone();
        newer[4] = 9;
        assert_eq!(
            ClothAsset::from_bytes(&newer).expect_err("newer").code(),
            "cloth_unsupported_version"
        );
        for cut in [3, 20, bytes.len() / 2, bytes.len() - 1] {
            assert!(ClothAsset::from_bytes(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn built_solver_pins_zero_reach_vertices_and_weighs_by_area() {
        let asset = ClothAsset::cook(&source(), &grid(3, 0.1), None, &[None]).expect("cooks");
        let world = Affine3A::from_translation(Vec3::new(0.0, 2.0, 0.0));
        let mut solver = asset.build_solver(0, 1.0 / 60.0, world).expect("builds");
        assert_eq!(solver.particles().len(), 9);
        assert_eq!(solver.particles()[0].inv_mass, 0.0);
        assert!(solver.particles()[4].inv_mass > 0.0);
        let top = solver.particles()[1].position;
        solver.step_fixed(30);
        assert_eq!(solver.particles()[1].position, top);
        assert!(solver.particles()[7].position.y < 2.0 - 0.2 + 1e-3);
    }

    #[test]
    fn render_binding_reproduces_render_vertices() {
        let mesh = grid(3, 0.1);
        let render = RenderMeshInput {
            positions: vec![
                Vec3::new(0.05, -0.05, 0.0),
                Vec3::new(0.12, -0.17, 0.01),
                Vec3::new(0.2, 0.0, -0.005),
            ],
            uvs: vec![[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]],
            triangles: vec![[0, 1, 2]],
        };
        let mut s = source();
        s.lods.clear();
        let asset = ClothAsset::cook(&s, &mesh, Some(&render), &[]).expect("cooks");
        let lod = asset.lod0();
        assert!(lod.render.from_render_mesh);
        let mut normals = Vec::new();
        vertex_normals(&lod.rest_positions, &lod.triangles, &mut normals);
        for (b, &p) in lod.render.vertices.iter().zip(&render.positions) {
            let (q, _) = bind_point(b, &lod.triangles, &lod.rest_positions, &normals);
            assert!(q.distance(p) < 1e-5, "{q} vs {p}");
        }
    }

    #[test]
    fn cook_refuses_bad_input() {
        let mut s = source();
        s.lods.clear();
        let mut mesh = grid(3, 0.1);
        s.maps.insert("sim_mix".into(), MapData::Dense(vec![0.5; 4]));
        assert_eq!(
            ClothAsset::cook(&s, &mesh, None, &[])
                .expect_err("short map")
                .code(),
            "cloth_map_size_mismatch"
        );
        s.maps.remove("sim_mix");
        s.constraint_sets.push(ConstraintSet {
            name: "v".into(),
            kind: ConstraintKind::Volume,
            compliance: 0.0,
            scale: None,
        });
        assert_eq!(
            ClothAsset::cook(&s, &mesh, None, &[]).expect_err("volume").code(),
            "cloth_asset_invalid"
        );
        s.constraint_sets.pop();
        mesh.triangles.push([0, 0, 1]);
        assert_eq!(
            ClothAsset::cook(&s, &mesh, None, &[]).expect_err("repeat").code(),
            "cloth_asset_invalid"
        );
        mesh.triangles.pop();
        mesh.joints = vec![[300, 0, 0, 0]; 9];
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 9];
        assert_eq!(
            ClothAsset::cook(&s, &mesh, None, &[]).expect_err("joint").code(),
            "cloth_asset_invalid"
        );
    }

    #[test]
    fn android_caps_authored_substeps_and_keeps_lower_ones() {
        let source = ClothParams {
            substeps: 8,
            ..ClothParams::default()
        };
        assert_eq!(source.for_platform(true).substeps, ANDROID_SUBSTEPS);
        assert_eq!(source.for_platform(false).substeps, 8);
        let low = ClothParams {
            substeps: 2,
            ..ClothParams::default()
        };
        assert_eq!(low.for_platform(true).substeps, 2);
        let soft = SoftParamsSource {
            substeps: 8,
            ..SoftParamsSource::default()
        };
        assert_eq!(soft.for_platform(true).substeps, ANDROID_SUBSTEPS);
        assert_eq!(soft.for_platform(false).substeps, 8);
    }

    #[test]
    fn weights_quantize_to_255() {
        let q = quantize_influence([1, 2, 3, 4], [1.0, 1.0, 1.0, 0.0]).expect("ok");
        assert_eq!(q.weights.iter().map(|&w| u32::from(w)).sum::<u32>(), 255);
        assert_eq!(q.joints, [1, 2, 3, 0]);
        let none = quantize_influence([0; 4], [0.0; 4]).expect("ok");
        assert_eq!(none.weights, [0; 4]);
    }

    fn octahedron() -> SimMeshInput {
        SimMeshInput {
            positions: vec![Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z],
            triangles: vec![
                [0, 2, 4],
                [2, 1, 4],
                [1, 3, 4],
                [3, 0, 4],
                [2, 0, 5],
                [1, 2, 5],
                [3, 1, 5],
                [0, 3, 5],
            ],
            ..SimMeshInput::default()
        }
    }

    fn soft_source(model: &str, maps: &str) -> SoftSource {
        SoftSource::from_ron_str(&format!(
            r#"(format_version: 1, name: "ball", model: {model},
                surface: (source: "ball.glb", node: "Ball"), maps: {{ {maps} }})"#,
        ))
        .expect("parses")
    }

    #[test]
    fn a_soft_surface_skin_survives_the_cooked_payload() {
        let source = soft_source("Pressure(pressure: 1.0, volume_compliance: 0.0)", "");
        let mut mesh = octahedron();
        mesh.joints = (0..6).map(|j| [j, 7, 0, 0]).collect();
        mesh.weights = vec![[0.5, 0.5, 0.0, 0.0]; 6];
        let asset = SoftAsset::cook(&source, &mesh, None).expect("cooks");
        assert!(asset.is_skinned());
        assert!(!asset.is_posed());
        assert_eq!(asset.skin[3].joints, [3, 7, 0, 0]);
        let bytes = asset.to_bytes().expect("writes");
        assert_eq!(SoftAsset::from_bytes(&bytes).expect("reads"), asset);

        let bare = SoftAsset::cook(&source, &octahedron(), None).expect("cooks");
        assert!(!bare.is_skinned());
        assert_eq!(
            SoftAsset::from_bytes(&bare.to_bytes().expect("writes")).expect("reads"),
            bare
        );

        // A skin block that does not cover the particles is refused.
        let mut short = asset;
        short.skin.pop();
        let err = SoftAsset::from_bytes(&short.to_bytes().expect("writes")).expect_err("short");
        assert_eq!(err.code(), "cloth_asset_invalid");
    }

    #[test]
    fn generated_tet_vertices_take_the_nearest_surface_skin() {
        // As many tet vertices as surface vertices, in another order: an equal count must not
        // read as "the same vertices".
        let positions = [
            Vec3::new(0.9, 0.0, 0.0),
            Vec3::new(-0.9, 0.0, 0.0),
            Vec3::new(0.0, 0.9, 0.0),
            Vec3::new(0.0, -0.9, 0.0),
            Vec3::new(0.0, 0.0, 0.9),
            Vec3::new(0.0, 0.0, -0.9),
        ];
        let tets = TetMeshInput {
            // Reversed against the surface order on purpose: vertex 0 is near surface vertex 1.
            positions: positions.iter().map(|p| -*p).collect(),
            tets: vec![[0, 2, 4, 5], [1, 4, 2, 5]],
            surface_binding: vec![
                TetBinding {
                    tet: 0,
                    weights: [1.0, 0.0, 0.0, 0.0],
                };
                6
            ],
        };
        let mut mesh = octahedron();
        mesh.joints = (0..6).map(|j| [j, 0, 0, 0]).collect();
        mesh.weights = vec![[1.0, 0.0, 0.0, 0.0]; 6];
        let source = soft_source(
            "Tetra(cell: Some(0.5), volume_compliance: 0.0)",
            r#""reach": Sparse(default: 10.0, entries: [(0, 0.0)])"#,
        );
        let asset = SoftAsset::cook(&source, &mesh, Some(&tets)).expect("cooks");
        assert_eq!(asset.skin[0].joints[0], 1, "vertex 0 sits by surface vertex 1");
        assert_eq!(asset.skin[1].joints[0], 0);
        assert_eq!(asset.skin[5].joints[0], 4);
        assert!(asset.is_posed());
    }

    #[test]
    fn a_mirrored_root_builds_soft_bodies_with_positive_volumes() {
        let mirror = Affine3A::from_scale(Vec3::new(-1.0, 1.0, 1.0));
        let pressure = SoftAsset::cook(
            &soft_source("Pressure(pressure: 1.0, volume_compliance: 0.0)", ""),
            &octahedron(),
            None,
        )
        .expect("cooks");
        let plain = pressure
            .build_solver(1.0 / 60.0, Affine3A::IDENTITY)
            .expect("builds");
        let mirrored = pressure
            .build_solver(1.0 / 60.0, mirror)
            .expect("mirrored builds");
        assert!(plain.rest_volume() > 0.0);
        assert!((mirrored.rest_volume() - plain.rest_volume()).abs() < 1e-5);
        assert!((mirrored.volume() - mirrored.rest_volume()).abs() < 1e-5);

        // Two tets over the octahedron's vertices, both of positive rest volume.
        let tets = TetMeshInput {
            positions: octahedron().positions,
            tets: vec![[0, 4, 2, 5], [1, 2, 4, 5]],
            surface_binding: vec![
                TetBinding {
                    tet: 0,
                    weights: [1.0, 0.0, 0.0, 0.0],
                };
                6
            ],
        };
        let tetra = SoftAsset::cook(
            &soft_source("Tetra(cell: Some(0.5), volume_compliance: 0.0)", ""),
            &octahedron(),
            Some(&tets),
        )
        .expect("cooks");
        let plain = tetra
            .build_solver(1.0 / 60.0, Affine3A::IDENTITY)
            .expect("builds");
        let mirrored = tetra.build_solver(1.0 / 60.0, mirror).expect("mirrored builds");
        assert!(plain.rest_volume() > 0.0);
        assert!((mirrored.rest_volume() - plain.rest_volume()).abs() < 1e-5);
        // The asset's own order is untouched (the glue binds the drawn surface through it).
        let SoftModelData::Tetra { tets, .. } = &tetra.model else {
            panic!("tetra model");
        };
        assert_eq!(tets[0], [0, 4, 2, 5]);
    }

    #[test]
    fn lod_maps_resample_by_nearest_vertex() {
        let mut s = source();
        s.maps.insert(
            "pose_pull".into(),
            MapData::Dense((0..9).map(|i| i as f32 / 10.0).collect()),
        );
        let coarse = grid(2, 0.2);
        let asset = ClothAsset::cook(&s, &grid(3, 0.1), None, &[Some(coarse)]).expect("cooks");
        // Coarse vertices sit on fine vertices 0, 2, 6, 8.
        assert_eq!(asset.lods[1].maps.pose_pull, vec![0.0, 0.2, 0.6, 0.8]);
        assert_eq!(asset.lods[1].rest_positions.len(), 4);
    }
}
