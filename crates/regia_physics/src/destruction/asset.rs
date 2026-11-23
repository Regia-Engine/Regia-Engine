// SPDX-License-Identifier: MPL-2.0
//! `.destructible.ron` v2: the runtime chunk asset of destruction level 2 (ADR-1597; v2 adds the
//! optional structural load and falling-section settings, ADR-2734).
//!
//! A destructible is a set of pre-fractured chunks (stable name, mass, convex collision hulls,
//! bounds, whether the world holds it up, render LODs) and the bonds between touching chunks
//! (contact area, anchor, normal and the impulse that breaks the bond). The desktop fracture tool writes it
//! (`regia_asset_pipeline::destructible_cook` turns a `.fracture.ron` into one); the runtime only
//! reads it. Every position is in the asset's space, metres. Format reference:
//! `docs/formats/destructible.md`.

use serde::{Deserialize, Serialize};

/// Version of the `.destructible.ron` shape ([`DestructibleAsset`]).
pub const DESTRUCTIBLE_FORMAT_VERSION: u32 = 2;

/// Most chunks one destructible may hold.
pub const MAX_DESTRUCTIBLE_CHUNKS: usize = 4096;

/// Type path of a cooked destructible sub-asset (the `.destructible.ron` text in the cooked
/// wrapper, written by the `.meta` fracture cook).
pub const DESTRUCTIBLE_TYPE_PATH: &str = "regia_physics::destruction::DestructibleAsset";

/// Most points one chunk's collision hull may hold.
pub const MAX_HULL_POINTS: usize = 256;

/// Most bonds one destructible may have (about 16 per chunk at the chunk limit).
pub const MAX_DESTRUCTIBLE_BONDS: usize = 65_536;

/// Longest `name` or `fx_tag` in bytes.
pub const MAX_DESTRUCTIBLE_STRING: usize = 256;

/// Most convex hulls one chunk's collision proxy may hold.
pub const MAX_HULLS_PER_CHUNK: usize = 16;

/// One destructible: chunks, bonds and the material that turns hits into bond damage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructibleAsset {
    /// Must be [`DESTRUCTIBLE_FORMAT_VERSION`].
    pub format_version: u32,
    /// Display name (also the agent-protocol name when no other is given).
    pub name: String,
    /// The kit (source model) this set fractures, and its revision; positions are in the kit's
    /// frame (metres, +Y up).
    #[serde(default)]
    pub source: Option<SourceKit>,
    /// The pre-scored fracture pattern this set used, if any.
    #[serde(default)]
    pub pattern_id: Option<String>,
    /// The fracture seed, so the set can be made again.
    #[serde(default)]
    pub fracture_seed: Option<u64>,
    /// The damage stage this set replaces (a game's `"D2"`), if it is part of a stage chain.
    #[serde(default)]
    pub replaces_stage: Option<String>,
    /// The `.fracture.ron` that holds the render meshes (chunk `i` here is chunk `i` there), or
    /// `None` when the chunks name their own LODs or the game draws them another way.
    #[serde(default)]
    pub meshes: Option<String>,
    /// Material of the chunks' outer (original surface) faces; usually the kit's own material.
    #[serde(default)]
    pub outer_material: Option<String>,
    /// Material of the cut faces.
    #[serde(default)]
    pub inner_material: Option<String>,
    /// How hits turn into bond damage, and the sound/VFX tag.
    pub material: DestructionMaterial,
    /// The chunks, in fracture order.
    pub chunks: Vec<DestructibleChunk>,
    /// One bond per touching pair, `a < b`, no pair twice.
    pub bonds: Vec<DestructibleBond>,
    /// Structural load (v2): gravity load relaxed through the bonds to the support chunks, so an
    /// overloaded bond fails without a hit. `None` (the default, and every v1 asset): bonds are
    /// loaded by hits only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<LoadSettings>,
    /// Falling sections (v2): a section that lost its support but keeps intact bonds falls as one
    /// body, and an object with no support chunks is a free body from its first tick. `None` (the
    /// default): a collapsing section breaks into its chunks and a free object stays put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub falling: Option<FallingSettings>,
}

/// How gravity loads the bonds of a destructible (`.destructible.ron` v2, ADR-2734).
///
/// Each chunk's weight (`mass * gravity`, along world -Y) flows through the intact bonds to the
/// support chunks. The flow is the one a resistive network gives (each bond conducts in
/// proportion to its area): a column carries the weight above each bond, an arch shares its load
/// between its feet. It is solved with `iterations` conjugate-gradient steps, only on a tick where
/// the structure's bonds changed (or on demand). A bond whose load exceeds its `max_load`
/// accumulates `(load - max_load) * failure_rate * dt` newton-seconds of damage per tick and breaks
/// at its `break_impulse`, like a hit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoadSettings {
    /// Gravity, m/s^2 along world -Y (>= 0; 9.81 on Earth).
    #[serde(default = "default_gravity")]
    pub gravity: f32,
    /// Conjugate-gradient iterations per solve (1 to [`MAX_LOAD_ITERATIONS`]). A chain of `n`
    /// unsupported chunks is exact after `n` iterations.
    #[serde(default = "default_load_iterations")]
    pub iterations: u32,
    /// Load (newtons) a bond carries without damage per square metre of its area, for bonds that
    /// name no `max_load` (> 0).
    pub max_load_per_area: f32,
    /// Damage per newton of overload per second (>= 0; 0 records the load but never breaks).
    #[serde(default = "default_failure_rate")]
    pub failure_rate: f32,
}

impl Default for LoadSettings {
    fn default() -> Self {
        Self {
            gravity: default_gravity(),
            iterations: default_load_iterations(),
            max_load_per_area: 1.0e6,
            failure_rate: default_failure_rate(),
        }
    }
}

fn default_gravity() -> f32 {
    9.81
}

fn default_load_iterations() -> u32 {
    64
}

fn default_failure_rate() -> f32 {
    1.0
}

/// Most conjugate-gradient iterations a [`LoadSettings`] may ask for.
pub const MAX_LOAD_ITERATIONS: u32 = 4096;

/// How unsupported sections and free objects move (`.destructible.ron` v2, ADR-2734).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FallingSettings {
    /// A section of at least this many chunks joined by intact bonds falls as one body (>= 2);
    /// smaller ones break into their chunks.
    #[serde(default = "default_min_section_chunks")]
    pub min_section_chunks: u32,
    /// An object with no support chunks is a free rigid body from its first tick (it falls, lands
    /// and can be knocked over); `false` keeps it fixed until it breaks, as without this setting.
    #[serde(default = "default_true")]
    pub free_body: bool,
}

impl Default for FallingSettings {
    fn default() -> Self {
        Self {
            min_section_chunks: default_min_section_chunks(),
            free_body: true,
        }
    }
}

fn default_min_section_chunks() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

/// How hits turn into bond damage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructionMaterial {
    /// Sound/VFX hook key carried by every break event (`"concrete"`, `"wood"`).
    pub fx_tag: String,
    /// Multiplier on every impulse that reaches the bonds (0 makes the object indestructible).
    pub damage_scale: f32,
    /// Impulses (newton-seconds) below this, per chunk and per hit, do no damage: chip damage
    /// from small hits never adds up.
    pub min_impulse: f32,
    /// Accumulated bond damage heals by this many newton-seconds per second (0: never).
    pub decay_per_second: f32,
    /// Largest spin (radians per second, per axis) a detached chunk gets from its seed.
    pub detach_spin: f32,
}

impl Default for DestructionMaterial {
    fn default() -> Self {
        Self {
            fx_tag: "default".to_string(),
            damage_scale: 1.0,
            min_impulse: 0.0,
            decay_per_second: 0.0,
            detach_spin: 2.0,
        }
    }
}

/// The kit a set fractures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceKit {
    /// Kit id.
    pub kit: String,
    /// Kit revision the set was cut from.
    pub revision: String,
}

/// One pre-fractured chunk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructibleChunk {
    /// Stable name (`frac_<kit>_<n>`); unique in the set when not empty.
    #[serde(default)]
    pub name: String,
    /// The kit part (or rig bone) this chunk belongs to, if the kit has several.
    #[serde(default)]
    pub parent: Option<String>,
    /// Centre of mass in the asset's space (also the chunk's pivot).
    pub centroid: [f32; 3],
    /// Mass in kilograms (> 0).
    pub mass: f32,
    /// Principal moments of inertia about `centroid` along the asset's axes (kg m^2, each > 0);
    /// `None` computes them from the hulls at uniform density.
    #[serde(default)]
    pub inertia: Option<[f32; 3]>,
    /// Collision proxy: 1 to [`MAX_HULLS_PER_CHUNK`] convex hulls, each 4 to [`MAX_HULL_POINTS`]
    /// points relative to `centroid`.
    pub hulls: Vec<Vec<[f32; 3]>>,
    /// Bounds minimum in the asset's space (navmesh invalidation uses the bounds).
    pub bounds_min: [f32; 3],
    /// Bounds maximum in the asset's space.
    pub bounds_max: [f32; 3],
    /// The world holds this chunk up (it rests on the ground or is built into a wall). Chunks
    /// that lose every path to a support chunk collapse.
    pub support: bool,
    /// Render mesh references, LOD0 first (LOD0 and LOD1 are enough); empty when the set-level
    /// `meshes` provides them.
    #[serde(default)]
    pub lods: Vec<String>,
    /// Sound/VFX tag of this chunk (`"glass"` in a concrete set); `None` uses the material's.
    #[serde(default)]
    pub fx_tag: Option<String>,
    /// A small chunk: it gets no debris body when it detaches (its sound/VFX hook still fires).
    #[serde(default)]
    pub small: bool,
    /// Seconds its debris body lives (overrides the budget's `max_lifetime_secs`).
    #[serde(default)]
    pub lifetime_secs: Option<f32>,
    /// Hot-edge emission of the cut faces, 0 to 1 (render data; passed through untouched).
    #[serde(default)]
    pub edge_heat: Option<f32>,
}

impl DestructibleChunk {
    /// The collision proxy: one convex hull, or a compound of the hulls.
    #[must_use]
    pub fn collider_shape(&self) -> crate::ColliderShape {
        use regia_core::math::Vec3;
        let hull = |points: &Vec<[f32; 3]>| crate::ColliderShape::ConvexHull {
            points: points.iter().map(|p| Vec3::from_array(*p)).collect(),
        };
        match self.hulls.as_slice() {
            [one] => hull(one),
            many => crate::ColliderShape::Compound {
                shapes: many
                    .iter()
                    .map(|h| (regia_scene::Transform::default(), hull(h)))
                    .collect(),
            },
        }
    }
}

/// A breakable bond between two touching chunks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DestructibleBond {
    /// Lower chunk index.
    pub a: u32,
    /// Higher chunk index.
    pub b: u32,
    /// Contact area, square metres.
    pub area: f32,
    /// Centre of the shared face in the asset's space.
    pub anchor: [f32; 3],
    /// Unit normal of the shared face, from `a` toward `b`.
    pub normal: [f32; 3],
    /// Accumulated impulse (newton-seconds) that breaks the bond (> 0).
    pub break_impulse: f32,
    /// Load (newtons) the bond carries without damage when the asset has [`LoadSettings`] (> 0);
    /// `None` uses `max_load_per_area * area`. Ignored without load settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_load: Option<f32>,
}

impl DestructibleBond {
    /// The load (newtons) this bond carries without damage under `load`.
    #[must_use]
    pub fn max_load_under(&self, load: &LoadSettings) -> f32 {
        self.max_load.unwrap_or(load.max_load_per_area * self.area)
    }
}

/// Why a destructible failed to load, validate or build. [`DestructibleError::code`] is the
/// agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum DestructibleError {
    /// RON syntax or shape error.
    #[error("destructible parse: {0}")]
    Parse(String),
    /// A `format_version` this engine cannot read (newer, or older with no migrator).
    #[error("destructible format_version {found} is not supported (this engine reads {expected})")]
    Version {
        /// Version found.
        found: u32,
        /// Version supported.
        expected: u32,
    },
    /// A validation rule failed.
    #[error("destructible `{name}`: {reason}")]
    Invalid {
        /// Asset name.
        name: String,
        /// Which rule.
        reason: String,
    },
}

impl DestructibleError {
    /// The agent-protocol error code: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse(_) => "destruction_parse",
            Self::Version { .. } => "destruction_version",
            Self::Invalid { .. } => "destruction_invalid_asset",
        }
    }
}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|c| c.is_finite())
}

impl DestructibleAsset {
    /// Parses, migrates (older versions; none exist at v1) and validates a `.destructible.ron`.
    ///
    /// # Errors
    /// [`DestructibleError::Parse`] for malformed text, [`DestructibleError::Version`] for a
    /// version this engine cannot read, [`DestructibleError::Invalid`] for a broken rule.
    pub fn from_ron(text: &str) -> Result<Self, DestructibleError> {
        let text = migrate_destructible(text)?;
        let asset: Self = ron::from_str(&text).map_err(|e| DestructibleError::Parse(e.to_string()))?;
        asset.validate()?;
        Ok(asset)
    }

    /// Reads a cooked destructible file ([`DESTRUCTIBLE_TYPE_PATH`]).
    ///
    /// # Errors
    /// [`DestructibleError::Parse`] for a bad wrapper or another asset type, otherwise as
    /// [`Self::from_ron`].
    pub fn from_cooked_file(bytes: &[u8]) -> Result<Self, DestructibleError> {
        let (header, payload) =
            regia_asset::decode_cooked_file(bytes).map_err(|e| DestructibleError::Parse(e.to_string()))?;
        if header.type_tag != regia_core::id::TypePathHash::of(DESTRUCTIBLE_TYPE_PATH) {
            return Err(DestructibleError::Parse(
                "the cooked file is another asset type".to_string(),
            ));
        }
        let text = std::str::from_utf8(payload).map_err(|e| DestructibleError::Parse(e.to_string()))?;
        Self::from_ron(text)
    }

    /// Loads the cooked destructible `id` through `server`: the latest mounted pak that holds it,
    /// else the cooked cache. Its id is marked as decoded outside the asset server
    /// ([`regia_asset::AssetServer::track_external`]), so a hot reload bumps
    /// [`regia_asset::AssetServer::generation`] for the caller to load it again.
    ///
    /// # Errors
    /// [`DestructibleError::Parse`] when no pak or cache file holds it, otherwise as
    /// [`Self::from_cooked_file`].
    pub fn load(
        server: &mut regia_asset::AssetServer,
        id: regia_asset::AssetId,
    ) -> Result<Self, DestructibleError> {
        let _span = tracing::info_span!("asset_load", kind = "destructible").entered();
        server.track_external(id);
        let (bytes, _) = server
            .read_cooked(id)
            .map_err(|e| DestructibleError::Parse(e.to_string()))?;
        Self::from_cooked_file(&bytes)
    }

    /// Pretty RON.
    ///
    /// # Errors
    /// [`DestructibleError::Parse`] when serialization fails.
    pub fn to_ron(&self) -> Result<String, DestructibleError> {
        ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| DestructibleError::Parse(e.to_string()))
    }

    /// Checks every rule of the format.
    ///
    /// # Errors
    /// [`DestructibleError::Version`] or [`DestructibleError::Invalid`] naming the first broken
    /// rule.
    pub fn validate(&self) -> Result<(), DestructibleError> {
        let fail = |reason: String| DestructibleError::Invalid {
            name: self.name.clone(),
            reason,
        };
        if self.format_version != DESTRUCTIBLE_FORMAT_VERSION {
            return Err(DestructibleError::Version {
                found: self.format_version,
                expected: DESTRUCTIBLE_FORMAT_VERSION,
            });
        }
        if self.name.is_empty() {
            return Err(fail("name is empty".into()));
        }
        if self.name.len() > MAX_DESTRUCTIBLE_STRING || self.material.fx_tag.len() > MAX_DESTRUCTIBLE_STRING {
            return Err(fail(format!(
                "name or fx_tag is longer than {MAX_DESTRUCTIBLE_STRING} bytes"
            )));
        }
        if self.bonds.len() > MAX_DESTRUCTIBLE_BONDS {
            return Err(fail(format!(
                "{} bonds (at most {MAX_DESTRUCTIBLE_BONDS})",
                self.bonds.len()
            )));
        }
        let m = &self.material;
        for (field, value) in [
            ("damage_scale", m.damage_scale),
            ("min_impulse", m.min_impulse),
            ("decay_per_second", m.decay_per_second),
            ("detach_spin", m.detach_spin),
        ] {
            if !(value.is_finite() && value >= 0.0) {
                return Err(fail(format!("material.{field} {value} must be finite and >= 0")));
            }
        }
        if self.chunks.is_empty() || self.chunks.len() > MAX_DESTRUCTIBLE_CHUNKS {
            return Err(fail(format!(
                "{} chunks (1..={MAX_DESTRUCTIBLE_CHUNKS} allowed)",
                self.chunks.len()
            )));
        }
        let mut names = std::collections::HashSet::with_capacity(self.chunks.len());
        for (i, c) in self.chunks.iter().enumerate() {
            if !(finite3(c.centroid) && finite3(c.bounds_min) && finite3(c.bounds_max)) {
                return Err(fail(format!("chunk {i} has a non-finite position")));
            }
            if !(c.mass.is_finite() && c.mass > 0.0) {
                return Err(fail(format!("chunk {i} mass {} must be > 0", c.mass)));
            }
            if c.hulls.is_empty() || c.hulls.len() > MAX_HULLS_PER_CHUNK {
                return Err(fail(format!(
                    "chunk {i} has {} hulls (1..={MAX_HULLS_PER_CHUNK} allowed)",
                    c.hulls.len()
                )));
            }
            for hull in &c.hulls {
                if hull.len() < 4 || hull.len() > MAX_HULL_POINTS {
                    return Err(fail(format!(
                        "chunk {i} has a hull of {} points (4..={MAX_HULL_POINTS} allowed)",
                        hull.len()
                    )));
                }
                if !hull.iter().all(|p| finite3(*p)) {
                    return Err(fail(format!("chunk {i} hull has a non-finite point")));
                }
            }
            if let Some(inertia) = c.inertia
                && !inertia.iter().all(|m| m.is_finite() && *m > 0.0)
            {
                return Err(fail(format!("chunk {i} inertia must be finite and > 0")));
            }
            if c.lifetime_secs.is_some_and(|t| !(t.is_finite() && t >= 0.0)) {
                return Err(fail(format!("chunk {i} lifetime_secs must be finite and >= 0")));
            }
            if c.edge_heat.is_some_and(|h| !(0.0..=1.0).contains(&h)) {
                return Err(fail(format!("chunk {i} edge_heat must be 0..=1")));
            }
            if !c.name.is_empty() && !names.insert(c.name.as_str()) {
                return Err(fail(format!("chunk {i} repeats the name `{}`", c.name)));
            }
            if (0..3).any(|k| c.bounds_min[k] > c.bounds_max[k]) {
                return Err(fail(format!("chunk {i} bounds_min is above bounds_max")));
            }
        }
        let n = self.chunks.len() as u32;
        let mut seen = std::collections::HashSet::with_capacity(self.bonds.len());
        for (i, b) in self.bonds.iter().enumerate() {
            if b.a >= b.b || b.b >= n {
                return Err(fail(format!(
                    "bond {i} ({}, {}) needs a < b < {n} (the chunk count)",
                    b.a, b.b
                )));
            }
            if !seen.insert((b.a, b.b)) {
                return Err(fail(format!("bond {i} repeats the pair ({}, {})", b.a, b.b)));
            }
            if !(b.break_impulse.is_finite() && b.break_impulse > 0.0) {
                return Err(fail(format!(
                    "bond {i} break_impulse {} must be > 0",
                    b.break_impulse
                )));
            }
            if !(b.area.is_finite() && b.area >= 0.0 && finite3(b.anchor) && finite3(b.normal)) {
                return Err(fail(format!("bond {i} has a non-finite or negative value")));
            }
            if b.max_load.is_some_and(|m| !(m.is_finite() && m > 0.0)) {
                return Err(fail(format!("bond {i} max_load must be finite and > 0")));
            }
        }
        if let Some(load) = &self.load {
            if !(load.gravity.is_finite() && load.gravity >= 0.0) {
                return Err(fail(format!(
                    "load.gravity {} must be finite and >= 0",
                    load.gravity
                )));
            }
            if load.iterations == 0 || load.iterations > MAX_LOAD_ITERATIONS {
                return Err(fail(format!(
                    "load.iterations {} must be 1..={MAX_LOAD_ITERATIONS}",
                    load.iterations
                )));
            }
            if !(load.max_load_per_area.is_finite() && load.max_load_per_area > 0.0) {
                return Err(fail(format!(
                    "load.max_load_per_area {} must be finite and > 0",
                    load.max_load_per_area
                )));
            }
            if !(load.failure_rate.is_finite() && load.failure_rate >= 0.0) {
                return Err(fail(format!(
                    "load.failure_rate {} must be finite and >= 0",
                    load.failure_rate
                )));
            }
            // Every weight must be a finite number of newtons for the solve to stay finite.
            if let Some(i) = self
                .chunks
                .iter()
                .position(|c| !(c.mass * load.gravity).is_finite())
            {
                return Err(fail(format!("chunk {i} weight is not finite under load.gravity")));
            }
        }
        if let Some(f) = &self.falling
            && f.min_section_chunks < 2
        {
            return Err(fail(format!(
                "falling.min_section_chunks {} must be >= 2",
                f.min_section_chunks
            )));
        }
        Ok(())
    }

    /// Indices of the support chunks, ascending.
    #[must_use]
    pub fn support_chunks(&self) -> Vec<u32> {
        (0..self.chunks.len() as u32)
            .filter(|&i| self.chunks[i as usize].support)
            .collect()
    }

    /// Level 1 cosmetic debris descriptions of every chunk ([`crate::DebrisSpawner`]): each chunk's
    /// hull at its centroid with its mass. Clients that only want the shattered look (no bonds, no
    /// support graph) spawn these in one go.
    #[must_use]
    pub fn chunk_descs(&self) -> Vec<crate::ChunkDesc> {
        use regia_core::math::{Quat, Vec3};
        self.chunks
            .iter()
            .map(|c| crate::ChunkDesc {
                local_translation: Vec3::from_array(c.centroid),
                local_rotation: Quat::IDENTITY,
                shape: c.collider_shape(),
                mass: c.mass,
            })
            .collect()
    }

    /// Total mass of all chunks, kilograms.
    #[must_use]
    pub fn total_mass(&self) -> f64 {
        self.chunks.iter().map(|c| f64::from(c.mass)).sum()
    }

    /// A box grid of `counts` cells (x fastest, then y, then z), each cell `cell` metres, the
    /// grid centred on x and z with its bottom at y = 0. Bonds join face neighbours; a bond's
    /// `break_impulse` is `strength_per_area` times the shared face's area. Walls, floors, bridge
    /// decks and brick piles are box grids; Voronoi sets come from the fracture tool.
    ///
    /// # Errors
    /// [`DestructibleError::Invalid`] for a zero count, a non-positive size, density or strength,
    /// more than [`MAX_DESTRUCTIBLE_CHUNKS`] cells, or an explicit support index out of range.
    pub fn box_grid(grid: &BoxGrid) -> Result<Self, DestructibleError> {
        let fail = |reason: String| DestructibleError::Invalid {
            name: grid.name.clone(),
            reason,
        };
        let [nx, ny, nz] = grid.counts;
        // Saturating: three counts near u32::MAX overflow u64 and could wrap to a small, accepted total.
        let total = u64::from(nx)
            .saturating_mul(u64::from(ny))
            .saturating_mul(u64::from(nz));
        if nx == 0 || ny == 0 || nz == 0 || total > MAX_DESTRUCTIBLE_CHUNKS as u64 {
            return Err(fail(format!(
                "grid counts {:?} must be >= 1 with at most {MAX_DESTRUCTIBLE_CHUNKS} cells",
                grid.counts
            )));
        }
        if !grid.cell.iter().all(|c| c.is_finite() && *c > 0.0) {
            return Err(fail("cell sizes must be > 0".into()));
        }
        if !(grid.density.is_finite() && grid.density > 0.0) {
            return Err(fail("density must be > 0".into()));
        }
        if !(grid.strength_per_area.is_finite() && grid.strength_per_area > 0.0) {
            return Err(fail("strength_per_area must be > 0".into()));
        }
        let [cx, cy, cz] = grid.cell;
        let half = [cx * 0.5, cy * 0.5, cz * 0.5];
        let index = |x: u32, y: u32, z: u32| x + nx * (y + ny * z);
        let hull: Vec<[f32; 3]> = (0..8u32)
            .map(|corner| {
                [
                    if corner & 1 == 0 { -half[0] } else { half[0] },
                    if corner & 2 == 0 { -half[1] } else { half[1] },
                    if corner & 4 == 0 { -half[2] } else { half[2] },
                ]
            })
            .collect();
        let mass = cx * cy * cz * grid.density;
        let mut chunks = Vec::with_capacity(total as usize);
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let centroid = [
                        (x as f32 + 0.5) * cx - nx as f32 * cx * 0.5,
                        (y as f32 + 0.5) * cy,
                        (z as f32 + 0.5) * cz - nz as f32 * cz * 0.5,
                    ];
                    let support = match &grid.support {
                        GridSupport::None => false,
                        GridSupport::Bottom => y == 0,
                        GridSupport::Ends => x == 0 || x == nx - 1,
                        GridSupport::Explicit(list) => list.contains(&index(x, y, z)),
                    };
                    chunks.push(DestructibleChunk {
                        name: format!("{}_{}", grid.name, index(x, y, z)),
                        parent: None,
                        centroid,
                        mass,
                        inertia: None,
                        hulls: vec![hull.clone()],
                        lods: Vec::new(),
                        fx_tag: None,
                        small: false,
                        lifetime_secs: None,
                        edge_heat: None,
                        bounds_min: [
                            centroid[0] - half[0],
                            centroid[1] - half[1],
                            centroid[2] - half[2],
                        ],
                        bounds_max: [
                            centroid[0] + half[0],
                            centroid[1] + half[1],
                            centroid[2] + half[2],
                        ],
                        support,
                    });
                }
            }
        }
        if let GridSupport::Explicit(list) = &grid.support
            && let Some(bad) = list.iter().find(|&&i| u64::from(i) >= total)
        {
            return Err(fail(format!("support index {bad} is outside the {total} cells")));
        }
        // Face neighbours, emitted per chunk in +x, +y, +z order: `a < b` and the list is sorted
        // by (a, b) because the +x neighbour has the smallest index step.
        let mut bonds = Vec::new();
        for z in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let a = index(x, y, z);
                    let centre = chunks[a as usize].centroid;
                    let mut push = |b: u32, axis: usize, area: f32| {
                        let mut anchor = centre;
                        anchor[axis] += half[axis];
                        let mut normal = [0.0; 3];
                        normal[axis] = 1.0;
                        bonds.push(DestructibleBond {
                            a,
                            b,
                            area,
                            anchor,
                            normal,
                            break_impulse: grid.strength_per_area * area,
                            max_load: None,
                        });
                    };
                    if x + 1 < nx {
                        push(index(x + 1, y, z), 0, cy * cz);
                    }
                    if y + 1 < ny {
                        push(index(x, y + 1, z), 1, cx * cz);
                    }
                    if z + 1 < nz {
                        push(index(x, y, z + 1), 2, cx * cy);
                    }
                }
            }
        }
        bonds.sort_by_key(|b| (b.a, b.b));
        let asset = Self {
            format_version: DESTRUCTIBLE_FORMAT_VERSION,
            name: grid.name.clone(),
            source: None,
            pattern_id: None,
            fracture_seed: None,
            replaces_stage: None,
            meshes: None,
            outer_material: None,
            inner_material: None,
            material: grid.material.clone(),
            chunks,
            bonds,
            load: None,
            falling: None,
        };
        asset.validate()?;
        Ok(asset)
    }
}

/// Which cells of a [`BoxGrid`] the world holds up.
#[derive(Clone, Debug, PartialEq)]
pub enum GridSupport {
    /// None: a free object (its largest piece stays whole when it breaks).
    None,
    /// The bottom layer (`y == 0`): walls and piles standing on the ground.
    Bottom,
    /// The first and last columns (`x == 0` and `x == nx - 1`): a deck resting on two abutments.
    Ends,
    /// These cell indices (`x + nx * (y + ny * z)`).
    Explicit(Vec<u32>),
}

/// Parameters of [`DestructibleAsset::box_grid`].
#[derive(Clone, Debug, PartialEq)]
pub struct BoxGrid {
    /// Asset name.
    pub name: String,
    /// Cells along x, y and z.
    pub counts: [u32; 3],
    /// Size of one cell, metres.
    pub cell: [f32; 3],
    /// Kilograms per cubic metre (concrete is about 2400).
    pub density: f32,
    /// Newton-seconds of accumulated impulse per square metre of bond area.
    pub strength_per_area: f32,
    /// Which cells are held by the world.
    pub support: GridSupport,
    /// The material.
    pub material: DestructionMaterial,
}

/// Brings an older `.destructible.ron` text to the current version; rejects a newer one. v1 → v2
/// only rewrites the version: v2 adds the optional `load`, `falling` and per-bond `max_load`, all
/// absent in a v1 file, so a migrated v1 asset behaves exactly as before.
///
/// # Errors
/// [`DestructibleError::Parse`] when the version cannot be read, [`DestructibleError::Version`]
/// for a version newer than the current one or older than v1.
pub fn migrate_destructible(text: &str) -> Result<String, DestructibleError> {
    let version = regia_core::version::peek_ron_version(text).map_err(DestructibleError::Parse)?;
    if version == DESTRUCTIBLE_FORMAT_VERSION {
        return Ok(text.to_string());
    }
    if version == 1 {
        return regia_core::version::rewrite_ron_version(text, "format_version", DESTRUCTIBLE_FORMAT_VERSION)
            .ok_or_else(|| DestructibleError::Parse("no top-level format_version to rewrite".to_string()));
    }
    Err(DestructibleError::Version {
        found: version,
        expected: DESTRUCTIBLE_FORMAT_VERSION,
    })
}

/// Versions [`migrate_destructible`] accepts below the current one.
#[must_use]
pub fn destructible_migration_sources() -> Vec<u32> {
    vec![1]
}

/// Serde field-list fingerprint of `.destructible.ron` (for `cargo xtask check-formats`).
#[must_use]
pub fn destructible_schema_fingerprint() -> String {
    use regia_core::schema::serde_field_names;
    regia_core::schema::join_field_lists(&[
        serde_field_names::<DestructibleAsset>(),
        serde_field_names::<SourceKit>(),
        serde_field_names::<DestructionMaterial>(),
        serde_field_names::<DestructibleChunk>(),
        serde_field_names::<DestructibleBond>(),
        serde_field_names::<LoadSettings>(),
        serde_field_names::<FallingSettings>(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> DestructibleAsset {
        DestructibleAsset::box_grid(&BoxGrid {
            name: "wall".into(),
            counts: [4, 3, 1],
            cell: [1.0, 0.5, 0.25],
            density: 2400.0,
            strength_per_area: 1000.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("grid")
    }

    #[test]
    fn grid_counts_bonds_and_supports() {
        let w = wall();
        assert_eq!(w.chunks.len(), 12);
        // 3 rows x 3 horizontal + 4 columns x 2 vertical.
        assert_eq!(w.bonds.len(), 9 + 8);
        assert_eq!(w.support_chunks(), vec![0, 1, 2, 3]);
        assert!(w.bonds.windows(2).all(|p| (p[0].a, p[0].b) < (p[1].a, p[1].b)));
        // A horizontal bond's face is 0.5 x 0.25.
        let b = &w.bonds[0];
        assert_eq!((b.a, b.b), (0, 1));
        assert!((b.area - 0.125).abs() < 1e-6);
        assert!((b.break_impulse - 125.0).abs() < 1e-3);
        assert!((w.total_mass() - 12.0 * 0.125 * 2400.0).abs() < 1e-3);
    }

    #[test]
    fn chunk_descs_feed_the_level_1_spawner() {
        let w = wall();
        let descs = w.chunk_descs();
        assert_eq!(descs.len(), w.chunks.len());
        assert_eq!(descs[5].mass, w.chunks[5].mass);
        assert_eq!(descs[5].local_translation.to_array(), w.chunks[5].centroid);
        assert!(matches!(&descs[0].shape, crate::ColliderShape::ConvexHull { points } if points.len() == 8));
        // Several hulls become one compound collider.
        let mut w = wall();
        let extra = w.chunks[0].hulls[0].clone();
        w.chunks[0].hulls.push(extra);
        assert!(matches!(
            w.chunks[0].collider_shape(),
            crate::ColliderShape::Compound { shapes } if shapes.len() == 2
        ));
    }

    #[test]
    fn ron_round_trip() {
        let w = wall();
        let text = w.to_ron().expect("ron");
        assert!(text.contains("format_version: 2"), "{text}");
        // No load or falling settings: the v2 text has no new fields.
        assert!(!text.contains("load") && !text.contains("falling"), "{text}");
        assert_eq!(DestructibleAsset::from_ron(&text).expect("parse"), w);
    }

    #[test]
    fn newer_and_older_versions_are_rejected_with_their_code() {
        let text = wall()
            .to_ron()
            .expect("ron")
            .replace("format_version: 2", "format_version: 3");
        let err = DestructibleAsset::from_ron(&text).expect_err("newer");
        assert_eq!(err.code(), "destruction_version");
        let text = wall()
            .to_ron()
            .expect("ron")
            .replace("format_version: 2", "format_version: 0");
        assert_eq!(
            migrate_destructible(&text).expect_err("older").code(),
            "destruction_version"
        );
        assert_eq!(
            DestructibleAsset::from_ron("(not ron").expect_err("bad").code(),
            "destruction_parse"
        );
        assert_eq!(destructible_migration_sources(), vec![1]);
    }

    #[test]
    fn a_v1_file_migrates_to_the_same_asset_with_no_load() {
        let w = wall();
        let v1 = w
            .to_ron()
            .expect("ron")
            .replace("format_version: 2", "format_version: 1");
        let migrated = migrate_destructible(&v1).expect("v1");
        assert!(migrated.contains("format_version: 2"));
        let back = DestructibleAsset::from_ron(&v1).expect("v1 loads");
        assert_eq!(back, w);
        assert!(back.load.is_none() && back.falling.is_none());
    }

    #[test]
    fn load_and_falling_settings_round_trip_and_validate() {
        let mut w = wall();
        w.load = Some(LoadSettings {
            max_load_per_area: 2.0e4,
            ..LoadSettings::default()
        });
        w.falling = Some(FallingSettings::default());
        w.bonds[0].max_load = Some(500.0);
        let text = w.to_ron().expect("ron");
        assert_eq!(DestructibleAsset::from_ron(&text).expect("parse"), w);
        assert_eq!(w.bonds[0].max_load_under(w.load.as_ref().expect("load")), 500.0);
        let area = w.bonds[1].area;
        assert!((w.bonds[1].max_load_under(w.load.as_ref().expect("load")) - 2.0e4 * area).abs() < 1e-3);
        for bad in [
            |a: &mut DestructibleAsset| a.load.as_mut().expect("load").iterations = 0,
            |a: &mut DestructibleAsset| a.load.as_mut().expect("load").gravity = f32::NAN,
            |a: &mut DestructibleAsset| a.load.as_mut().expect("load").gravity = -1.0,
            |a: &mut DestructibleAsset| a.load.as_mut().expect("load").max_load_per_area = 0.0,
            |a: &mut DestructibleAsset| a.load.as_mut().expect("load").failure_rate = f32::INFINITY,
            |a: &mut DestructibleAsset| a.load.as_mut().expect("load").gravity = f32::MAX,
            |a: &mut DestructibleAsset| a.bonds[2].max_load = Some(0.0),
            |a: &mut DestructibleAsset| a.falling.as_mut().expect("falling").min_section_chunks = 1,
        ] {
            let mut a = w.clone();
            bad(&mut a);
            assert_eq!(a.validate().expect_err("bad").code(), "destruction_invalid_asset");
        }
    }

    #[test]
    fn validation_rules() {
        let mut w = wall();
        w.bonds[0].break_impulse = 0.0;
        assert_eq!(
            w.validate().expect_err("zero").code(),
            "destruction_invalid_asset"
        );
        let mut w = wall();
        w.bonds.push(w.bonds[0].clone());
        assert!(w.validate().is_err(), "duplicate pair");
        let mut w = wall();
        w.bonds[0].a = 5;
        w.bonds[0].b = 2;
        assert!(w.validate().is_err(), "a > b");
        let mut w = wall();
        w.chunks[3].mass = f32::NAN;
        assert!(w.validate().is_err(), "nan mass");
        let mut w = wall();
        w.chunks[0].hulls[0].truncate(3);
        assert!(w.validate().is_err(), "short hull");
        let mut w = wall();
        w.chunks[0].hulls.clear();
        assert!(w.validate().is_err(), "no hull");
        let mut w = wall();
        w.chunks[1].name = w.chunks[0].name.clone();
        assert!(w.validate().is_err(), "repeated name");
        let mut w = wall();
        w.chunks[1].inertia = Some([1.0, 0.0, 1.0]);
        assert!(w.validate().is_err(), "zero inertia");
        let mut w = wall();
        w.chunks[1].edge_heat = Some(2.0);
        assert!(w.validate().is_err(), "edge heat");
        let mut w = wall();
        w.chunks[1].lifetime_secs = Some(-1.0);
        assert!(w.validate().is_err(), "lifetime");
        let mut w = wall();
        w.material.damage_scale = -1.0;
        assert!(w.validate().is_err(), "negative scale");
        let mut w = wall();
        w.chunks.clear();
        w.bonds.clear();
        assert!(w.validate().is_err(), "empty");
        let mut grid = BoxGrid {
            name: "g".into(),
            counts: [2, 1, 1],
            cell: [1.0; 3],
            density: 1.0,
            strength_per_area: 1.0,
            support: GridSupport::Explicit(vec![7]),
            material: DestructionMaterial::default(),
        };
        assert!(
            DestructibleAsset::box_grid(&grid).is_err(),
            "support out of range"
        );
        grid.support = GridSupport::Ends;
        grid.counts = [0, 1, 1];
        assert!(DestructibleAsset::box_grid(&grid).is_err(), "zero count");
    }

    #[test]
    fn box_grid_counts_that_overflow_u64_are_refused_not_wrapped() {
        for counts in [
            [u32::MAX, u32::MAX, u32::MAX],
            [u32::MAX, u32::MAX, 2],
            [1 << 31, 1 << 31, 1 << 31],
            [u32::MAX, 1, 1],
        ] {
            let grid = BoxGrid {
                name: "huge".into(),
                counts,
                cell: [1.0; 3],
                density: 1.0,
                strength_per_area: 1.0,
                support: GridSupport::Bottom,
                material: DestructionMaterial::default(),
            };
            assert!(DestructibleAsset::box_grid(&grid).is_err(), "{counts:?}");
        }
    }

    #[test]
    fn fingerprint_lists_every_struct() {
        let fp = destructible_schema_fingerprint();
        for needle in [
            "format_version",
            "meshes",
            "fx_tag",
            "detach_spin",
            "hulls",
            "support",
            "break_impulse",
            "revision",
            "lods",
            "inner_material",
            "edge_heat",
        ] {
            assert!(fp.contains(needle), "{needle} missing from {fp}");
        }
    }

    #[test]
    fn error_codes_follow_the_protocol_grammar() {
        let errors = [
            DestructibleError::Parse(String::new()),
            DestructibleError::Version {
                found: 0,
                expected: 1,
            },
            DestructibleError::Invalid {
                name: String::new(),
                reason: String::new(),
            },
        ];
        for e in errors {
            assert!(
                e.code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                e.code()
            );
        }
    }
}
