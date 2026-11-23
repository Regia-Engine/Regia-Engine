// SPDX-License-Identifier: MPL-2.0
//! Data model of the `.vfx.ron` effect asset — **frozen format v8** (`docs/formats/vfx-ron.md`).
//!
//! A [`VfxSystemDef`] groups emitters with timing ("core flash" first, smoke later, …). Each emitter
//! is a stack of modules in four stages:
//!
//! - **Spawn** ([`SpawnModule`]): how many particles, when — continuous rate, one-shot burst, or one
//!   per distance travelled (dust trails).
//! - **Init** ([`InitModule`]): where particles appear (shape) and their starting attributes —
//!   velocity, lifetime, size, rotation, colour.
//! - **Update** ([`UpdateModule`]): forces (gravity, drag, curl noise, vortex, attractor), curves over
//!   life (size / colour / alpha / rotation), a world-Y kill plane (`kill_below`), a box or
//!   sphere kill volume (v7, [`KillVolumeDef`]) and depth-buffer collision. v6 adds the scene
//!   wind (`wind_response`, `wind_drag`) and event spawns: `events` fire a child emitter of the
//!   system (spawn mode [`SpawnMode::Event`]) when a particle spawns, dies or hits the depth
//!   buffer. v7 adds the mesh-surface and skinned-mesh init shapes ([`ShapeDef::MeshSurface`],
//!   [`ShapeDef::SkinnedMesh`]) and the along-normal velocity ([`VelocityMode::Normal`]).
//! - **Render** ([`RenderModule`]): billboard / stretched billboard / mesh particles / ribbons
//!   (v7, [`RenderMode::Ribbon`] with [`RibbonDef`]), sprite or flipbook, blend mode,
//!   soft-particle depth fade, optional light emission. v8 adds [`RenderModule::mesh`]: a
//!   [`RenderMode::Mesh`] emitter draws a registered mesh asset instead of the built-in cube.
//!
//! Everything is plain `serde` + RON. The file is *frozen*: every struct uses
//! `#[serde(deny_unknown_fields)]` and the loader rejects a newer version, so additive changes need a
//! version bump and a migrator (`docs/specs/12-assets.md` § 4a). Ranges are written
//! `(min: a, max: b)`; a range is always written as the explicit two-field struct.
//!
//! The runtime (CPU) representation lives in [`crate::sim`]; this module is data only.

use crate::events::MAX_PACKED_EVENT_COUNT;
use crate::sim::MAX_INSTANCE_INTENSITY;
use regia_core::math::Vec3;
use regia_core::version::{self, VersionStep};
use serde::{Deserialize, Serialize};

/// Version of the `.vfx.ron` format this crate reads and writes.
pub const VFX_FORMAT_VERSION: u32 = 8;

/// Most points one ribbon strip keeps ([`RibbonDef::max_points`], v7).
pub const MAX_RIBBON_POINTS: u32 = 1024;

/// Largest [`ShapeDef::SkinnedMesh`] `max_per_frame` (v7).
pub const MAX_SKINNED_SPAWNS_PER_FRAME: u32 = 4096;

/// Most [`EventSpawnDef`]s one emitter may have (the GPU link table holds this many per emitter).
pub const MAX_EVENTS_PER_EMITTER: usize = 4;

/// Largest [`EventSpawnDef::count`]: one firing spawns at most this many children before the
/// instance's intensity scales it (intensity caps at 4).
pub const MAX_EVENT_COUNT: u32 = 64;

/// How many links an event chain may nest: parent to child is 1.
pub const MAX_EVENT_DEPTH: usize = 4;

/// Errors from parsing a `.vfx.ron` asset.
#[derive(Debug, thiserror::Error)]
pub enum VfxFormatError {
    /// RON syntax / serde failure. Carries the serde path context when available.
    #[error("vfx parse error: {0}")]
    Parse(#[from] ron::error::SpannedError),
    /// Wrong `version` field.
    #[error("vfx asset has version {found}, but this engine reads version {expected}")]
    Version {
        /// Version found in the file.
        found: u32,
        /// Version this build reads ([`VFX_FORMAT_VERSION`]).
        expected: u32,
    },
    /// A migration step failed or is missing.
    #[error("vfx migration failed: {0}")]
    Migration(String),
    /// Semantically invalid content (validated after parse).
    #[error("vfx asset invalid: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------------
// Ranges
// ---------------------------------------------------------------------------

/// An inclusive f32 range; sampled uniformly by Init modules.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangeDef {
    /// Lower bound.
    pub min: f32,
    /// Upper bound (>= `min` is validated).
    pub max: f32,
}

impl Default for RangeDef {
    fn default() -> Self {
        Self { min: 0.0, max: 0.0 }
    }
}

impl From<f32> for RangeDef {
    fn from(v: f32) -> Self {
        Self { min: v, max: v }
    }
}

/// A key on a scalar curve: `(time, value)` with linear interpolation between keys.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloatKey {
    /// Curve time in `[0, 1]` (normalised to particle life).
    pub t: f32,
    /// Value at `t`.
    pub v: f32,
}

/// A key on a colour gradient: `(time, "#rrggbb" or "#rrggbbaa")`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColorKey {
    /// Curve time in `[0, 1]` (normalised to particle life).
    pub t: f32,
    /// sRGB hex colour, parsed by [`regia_core::math::Color::from_hex`].
    pub color: String,
}

// ---------------------------------------------------------------------------
// System (the asset root)
// ---------------------------------------------------------------------------

/// The asset root of a `.vfx.ron` file: a named group of emitters with timing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VfxSystemDef {
    /// Format version; must be [`VFX_FORMAT_VERSION`].
    pub version: u32,
    /// Stable name the event router and scripts refer to (`SpawnEffect`).
    pub name: String,
    /// Pooling and LOD for the whole system.
    #[serde(default)]
    pub system: SystemModule,
    /// The emitters, played together when the system is triggered.
    pub emitters: Vec<EmitterDef>,
}

/// System-level module: budgets and distance LOD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemModule {
    /// Maximum live particles per active instance of this system (across its emitters).
    #[serde(default = "default_system_budget")]
    pub budget: u32,
    /// Optional distance LOD: `distances` are band boundaries (metres, increasing),
    /// `rate_scale` has one entry per band (and one more than `distances`). Fractions scale
    /// spawn rates; `0.0` disables the system in that band.
    #[serde(default)]
    pub lod: Option<LodDef>,
    /// Authored fixed bounds (v8 fold, ADR-8105 row (h)): when set, the instance's culling volume
    /// and the editor's bounds readout use this box (transformed by the instance) instead of the
    /// computed bound. Particles outside it are neither killed nor clipped (that is the v7 kill
    /// volume). `None` keeps the computed bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<FixedBoundsDef>,
}

impl Default for SystemModule {
    fn default() -> Self {
        Self {
            budget: default_system_budget(),
            lod: None,
            bounds: None,
        }
    }
}

/// An authored axis-aligned box in the system's local space (v8 fold): metres, relative to the
/// instance origin, before the instance transform (rotation and scale). `min <= max` on every
/// axis and every value finite (the loader refuses anything else).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedBoundsDef {
    /// Lower corner `[x, y, z]`, metres.
    pub min: [f32; 3],
    /// Upper corner `[x, y, z]`, metres.
    pub max: [f32; 3],
}

impl FixedBoundsDef {
    /// Whether every value is finite and `min <= max` on every axis.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.min
            .iter()
            .zip(&self.max)
            .all(|(lo, hi)| lo.is_finite() && hi.is_finite() && lo <= hi)
    }

    /// The box centre in local space (halved before the sum, so a valid box near `f32::MAX`
    /// does not overflow).
    #[must_use]
    pub fn center(&self) -> Vec3 {
        Vec3::from_array(self.min) * 0.5 + Vec3::from_array(self.max) * 0.5
    }

    /// Half the box size on each local axis (halved before the difference: finite for every
    /// valid box).
    #[must_use]
    pub fn half_extents(&self) -> Vec3 {
        Vec3::from_array(self.max) * 0.5 - Vec3::from_array(self.min) * 0.5
    }
}

/// Distance LOD bands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodDef {
    /// Band boundaries in metres (increasing).
    pub distances: Vec<f32>,
    /// Spawn-rate scale per band: `distances.len() + 1` entries.
    pub rate_scale: Vec<f32>,
}

fn default_system_budget() -> u32 {
    4096
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// One emitter: a Spawn + Init + Update + Render module stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitterDef {
    /// Name, unique within the system (referenced by tests and the editor).
    pub name: String,
    /// Seconds after the system triggers before this emitter activates.
    #[serde(default)]
    pub delay: f32,
    /// How long the emitter spawns (rate/per-distance); `None` = until the instance is stopped.
    /// Bursts fire once per activation regardless.
    #[serde(default)]
    pub duration: Option<f32>,
    /// The spawn module.
    pub spawn: SpawnModule,
    /// The init module.
    pub init: InitModule,
    /// The update module.
    #[serde(default)]
    pub update: UpdateModule,
    /// The render module.
    pub render: RenderModule,
}

// ---------------------------------------------------------------------------
// Spawn stage
// ---------------------------------------------------------------------------

/// Spawn module: particle counts over time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnModule {
    /// The spawn mode: rate, burst or per-distance.
    pub mode: SpawnMode,
    /// Particle lifetime in seconds.
    pub lifetime: RangeDef,
    /// Multiplier applied to spawn counts (authoring convenience; LOD scales on top).
    #[serde(default = "default_one")]
    pub count_scale: f32,
}

/// How particles are scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SpawnMode {
    /// `N` particles per second, continuously while active.
    Rate(f32),
    /// A one-shot count when the emitter activates; it fires once per activation.
    Burst(u32),
    /// One particle per `every` metres the instance origin moves (dust trails).
    PerDistance {
        /// Metres of travel between spawns.
        every: f32,
    },
    /// Spawns only when another emitter's [`EventSpawnDef`] names it (v6). The emitter has no
    /// schedule of its own: `delay` and `duration` must stay at their defaults, and it keeps no
    /// instance alive except while its parents' particles can still fire it.
    Event,
}

// ---------------------------------------------------------------------------
// Init stage
// ---------------------------------------------------------------------------

/// Init module: spawn shape and starting attributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitModule {
    /// Where particles spawn.
    #[serde(default)]
    pub shape: ShapeDef,
    /// Initial velocity.
    #[serde(default)]
    pub velocity: VelocityDef,
    /// Particle base size (world metres).
    pub size: RangeDef,
    /// Initial rotation (radians).
    #[serde(default)]
    pub rotation: RangeDef,
    /// Rotation speed (radians/s), applied over life.
    #[serde(default)]
    pub spin: RangeDef,
    /// Initial colour gradient sampled at spawn (`t = 0`).
    #[serde(default)]
    pub color: Vec<ColorKey>,
}

/// Emitter shapes. Positions are sampled in the instance's local frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum ShapeDef {
    /// A single point (the instance origin).
    #[default]
    Point,
    /// A sphere (volume, or shell when `surface`).
    Sphere {
        /// Radius range.
        radius: RangeDef,
        /// Sample only on the surface when `true`.
        #[serde(default)]
        surface: bool,
    },
    /// An axis-aligned box (volume, or shell when `surface`), centred on the origin.
    Box {
        /// Half extents per axis.
        half_extents: (f32, f32, f32),
        /// Sample only on the surface when `true`.
        #[serde(default)]
        surface: bool,
    },
    /// A cone along local +Z: point at the apex (origin), disc at `length` with `angle` half-angle.
    Cone {
        /// Half-angle in degrees (0 = beam).
        angle: f32,
        /// Distance range from apex along the axis.
        length: RangeDef,
    },
    /// The surface of a mesh asset (v7): a triangle is picked with probability proportional to
    /// its area (an alias table built when the mesh is registered, not per spawn), then a
    /// uniform point on it. The mesh's object space is the instance's local frame; the spawn
    /// normal is the triangle's face normal. The host registers the mesh under `mesh`
    /// ([`crate::scene::VfxLibrary::add_mesh`]); an unregistered, empty or zero-area mesh spawns
    /// nothing and reports `missing_mesh` / `bad_mesh` (it never panics).
    MeshSurface {
        /// Asset name the mesh was registered under.
        mesh: String,
    },
    /// The deformed surface of a skinned mesh asset (v7): sampled like [`Self::MeshSurface`] by
    /// bind-pose area, then the triangle's corners are skinned with the instance's current joint
    /// palette ([`crate::sim::VfxInstance::set_skin_palette`], linear blend of the four strongest
    /// influences) on the CPU. At most `max_per_frame` particles spawn per frame from this
    /// emitter; the GPU receives the deformed positions through the emit stage like any spawn.
    /// Without a palette the bind pose is sampled.
    SkinnedMesh {
        /// Asset name the mesh was registered under (it must carry a skin).
        mesh: String,
        /// Most spawns per frame, `1..=`[`MAX_SKINNED_SPAWNS_PER_FRAME`].
        #[serde(default = "default_skinned_per_frame")]
        max_per_frame: u32,
    },
}

impl ShapeDef {
    /// The mesh asset name of a mesh shape.
    #[must_use]
    pub fn mesh_name(&self) -> Option<&str> {
        match self {
            Self::MeshSurface { mesh } | Self::SkinnedMesh { mesh, .. } => Some(mesh),
            _ => None,
        }
    }
}

/// Initial velocity mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VelocityMode {
    /// Direction of the unrotated local spawn offset (ADR-1398), times `speed`.
    ///
    /// The instance basis does not rotate it. On the default basis (forward +Z, up +Y) a local
    /// +X spawn sits on world −X and this mode flies it back along local +X. Use [`Self::Planar`]
    /// for a ring along the rotated ground and [`Self::Normal`] for the rotated spawn normal.
    Radial {
        /// Speed range in m/s.
        speed: RangeDef,
    },
    /// A fixed direction in the instance frame, randomised inside a half-angle cone.
    Directional {
        /// Direction (normalised when sampled; the loaded value keeps the authored length).
        dir: (f32, f32, f32),
        /// Half-angle spread in degrees.
        #[serde(default)]
        spread: f32,
        /// Speed range in m/s.
        speed: RangeDef,
    },
    /// Outward in the instance's plane (v5): away from the local +Z axis through the origin,
    /// perpendicular to it, plus `lift` m/s along +Z. Played with local +Z on the ground
    /// normal, this is a horizontal ring that runs out along the ground (a blast's dust sheet,
    /// a wall jet). The direction is the spawn offset rotated into the instance frame; a spawn
    /// on the axis gets a random direction in the plane.
    Planar {
        /// Outward speed range in m/s.
        speed: RangeDef,
        /// Speed along local +Z in m/s (negative is toward the ground).
        #[serde(default)]
        lift: RangeDef,
    },
    /// Along the spawn normal (v7): the face normal of a mesh shape, the outward direction of a
    /// sphere or box, the apex-to-point direction of a cone, local +Z for a point. Rotated by the
    /// instance frame.
    Normal {
        /// Speed range in m/s.
        speed: RangeDef,
    },
}

/// Initial velocity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VelocityDef {
    /// The mode.
    pub mode: VelocityMode,
    /// Fraction of the instance's world velocity inherited (tracer carried by a moving barrel).
    #[serde(default)]
    pub inherit: f32,
}

impl Default for VelocityDef {
    fn default() -> Self {
        Self {
            mode: VelocityMode::Radial {
                speed: RangeDef::from(1.0),
            },
            inherit: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Update stage
// ---------------------------------------------------------------------------

/// Update module: forces, curves over life, kills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateModule {
    /// Constant acceleration (m/s²).
    #[serde(default)]
    pub gravity: (f32, f32, f32),
    /// Velocity damping per second (0 = none; `v *= 1 / (1 + drag * dt)`).
    #[serde(default)]
    pub drag: f32,
    /// Curl-noise turbulence: `(amount, frequency, speed)` of the noise field.
    #[serde(default)]
    pub curl: Option<(f32, f32, f32)>,
    /// Vortex around an axis through the instance origin.
    #[serde(default)]
    pub vortex: Option<VortexDef>,
    /// Point attractor.
    #[serde(default)]
    pub attractor: Option<AttractorDef>,
    /// Size over life (`t` in `[0,1]`, multiplier of the spawn size).
    #[serde(default)]
    pub size_over_life: Vec<FloatKey>,
    /// Colour over life (multiplies the initial colour's RGB; the key alpha is ignored, fade with
    /// `alpha_over_life`).
    #[serde(default)]
    pub color_over_life: Vec<ColorKey>,
    /// Alpha over life (multiplies the spawn alpha).
    #[serde(default)]
    pub alpha_over_life: Vec<FloatKey>,
    /// Rotation over life (additive radians).
    #[serde(default)]
    pub rotation_over_life: Vec<FloatKey>,
    /// Kill particles whose world Y drops below this plane (grounding smoke).
    #[serde(default)]
    pub kill_below: Option<f32>,
    /// Collide against the scene depth buffer: kill or slide at surfaces.
    #[serde(default)]
    pub depth_collision: Option<DepthCollisionDef>,
    /// How strongly the scene wind carries these particles, `0..=1` (v6). The coupling rate to
    /// the air is `wind_response * wind_drag` per second; `0` ignores the wind and leaves the
    /// simulation bit-identical to a file without it.
    #[serde(default)]
    pub wind_response: f32,
    /// Linear drag toward the wind's velocity, per second, at `wind_response` 1 (v6). Air-relative,
    /// on top of the still-air [`Self::drag`]: `dv/dt = g - drag * v + c * (wind - v)` with
    /// `c = wind_response * wind_drag`, so the terminal velocity is
    /// `(g + c * wind) / (drag + c)`.
    #[serde(default = "default_one")]
    pub wind_drag: f32,
    /// Child spawns these particles fire (v6), at most [`MAX_EVENTS_PER_EMITTER`].
    #[serde(default)]
    pub events: Vec<EventSpawnDef>,
    /// A box or sphere that kills the particles inside it, or outside it (v7). Tested after
    /// each step's move, like `kill_below`; a kill fires the `Death` event.
    #[serde(default)]
    pub kill_volume: Option<KillVolumeDef>,
}

/// A kill volume (v7): placed in the instance frame like [`AttractorDef::position`] and scaled
/// with the instance ([`crate::sim::VfxInstance::set_scale`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillVolumeDef {
    /// The volume's shape.
    pub shape: KillShape,
    /// Centre in the instance frame (metres from the origin along right, up, forward).
    #[serde(default)]
    pub center: (f32, f32, f32),
    /// Which side kills.
    #[serde(default)]
    pub kill: KillSide,
}

/// Shape of a [`KillVolumeDef`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum KillShape {
    /// A box along the instance axes; a point on its faces is inside.
    Box {
        /// Half extents along right, up, forward (each `> 0`).
        half_extents: (f32, f32, f32),
    },
    /// A sphere; a point on its surface is inside.
    Sphere {
        /// Radius in metres (`> 0`).
        radius: f32,
    },
}

/// Which side of a [`KillVolumeDef`] kills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum KillSide {
    /// Particles that enter the volume die (a wall, water).
    #[default]
    Inside,
    /// Particles that leave the volume die (a room, a bounds box).
    Outside,
}

impl Default for UpdateModule {
    fn default() -> Self {
        Self {
            gravity: (0.0, 0.0, 0.0),
            drag: 0.0,
            curl: None,
            vortex: None,
            attractor: None,
            size_over_life: Vec::new(),
            color_over_life: Vec::new(),
            alpha_over_life: Vec::new(),
            rotation_over_life: Vec::new(),
            kill_below: None,
            depth_collision: None,
            wind_response: 0.0,
            wind_drag: 1.0,
            events: Vec::new(),
            kill_volume: None,
        }
    }
}

/// What makes a particle fire an [`EventSpawnDef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventTrigger {
    /// The particle's first simulated step.
    Spawn,
    /// The particle ended: old age, the `kill_below` plane or a depth-collision kill.
    Death,
    /// The particle went through the depth buffer. A sliding particle fires on every simulated
    /// step it stays in contact; a killing collision fires this and then [`Self::Death`].
    Collision,
}

/// A child spawn: when a particle of this emitter hits the trigger, `count` particles of the child
/// emitter appear at its position on the next simulated frame (v6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventSpawnDef {
    /// The trigger.
    pub on: EventTrigger,
    /// Name of the child emitter in the same system; its spawn mode must be [`SpawnMode::Event`].
    pub child: String,
    /// Children per firing, `1..=`[`MAX_EVENT_COUNT`]. The child's `count_scale`, the LOD band and
    /// the instance's intensity scale it, rounded like a burst.
    pub count: u32,
    /// Chance that one firing spawns its `count`, `0..=1`. Drawn from a hash of the particle and
    /// the frame, so a replay fires the same events.
    #[serde(default = "default_one")]
    pub probability: f32,
    /// Fraction of the parent's velocity added to each child's sampled velocity, `0..=1`.
    #[serde(default)]
    pub inherit_velocity: f32,
}

/// Vortex force around an axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VortexDef {
    /// Rotation axis in the instance frame (normalised when uploaded; the loaded value keeps the authored length).
    pub axis: (f32, f32, f32),
    /// Tangential acceleration (m/s²) at 1 m from the axis.
    pub strength: f32,
    /// Extra radial acceleration (m/s², inward when negative), added to the built-in outward
    /// `0.25 × strength` of the vortex. It acts even when `strength` is 0. Before ADR-5629 the
    /// field was loaded but ignored by the GPU kernel and the CPU ribbons.
    #[serde(default)]
    pub radial: f32,
}

/// Point attractor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttractorDef {
    /// Attractor centre in the instance frame.
    pub position: (f32, f32, f32),
    /// Acceleration (m/s²) toward the centre, pulls with `strength / (r² + 1)` (finite at the centre).
    pub strength: f32,
}

/// Depth-buffer collision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepthCollisionDef {
    /// `true` = kill the particle at the surface; `false` = project it onto the surface.
    pub kill: bool,
    /// World-metre tolerance before the collision triggers (soft-particle-like).
    #[serde(default = "default_one")]
    pub offset: f32,
}

// ---------------------------------------------------------------------------
// Render stage
// ---------------------------------------------------------------------------

/// Render module: how particles are drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderModule {
    /// Geometry mode.
    pub mode: RenderMode,
    /// Which procedural sprite tile to use (see `docs/formats/vfx-ron.md`).
    #[serde(default)]
    pub sprite: SpriteId,
    /// Flipbook animation over the sprite atlas row.
    #[serde(default)]
    pub flipbook: Option<FlipbookDef>,
    /// Blend mode. Additive emitters are drawn unsorted before alpha emitters.
    #[serde(default)]
    pub blend: BlendMode,
    /// Soft-particle depth fade width in world metres (0 = off).
    #[serde(default)]
    pub softness: f32,
    /// Global alpha multiplier.
    #[serde(default = "default_one")]
    pub alpha_scale: f32,
    /// Stretched-billboard parameters (used by [`RenderMode::Stretched`]).
    #[serde(default)]
    pub stretch: StretchDef,
    /// Light emitted by a sample of particles, capped by the profile's light budget.
    #[serde(default)]
    pub light: Option<ParticleLightDef>,
    /// HDR multiplier for unlit (emissive) particles. `1` leaves the authored colour.
    /// Lit particles use this as an albedo scale instead of a light source.
    #[serde(default = "default_one")]
    pub emissive: f32,
    /// Shade the billboard as a sphere from the scene sun and ambient (smoke).
    #[serde(default)]
    pub lit: bool,
    /// Top-lit self-shadow strength for [`Self::lit`] puffs, `0` = off, `1` = black at the bottom.
    #[serde(default = "default_self_shadow")]
    pub self_shadow: f32,
    /// Minimum on-screen size in pixels. `0` uses the profile floor.
    #[serde(default)]
    pub screen_floor_px: f32,
    /// Heat-haze strength in pixels (v4). Above `0` the particle draws no colour: it bends the
    /// scene behind it by up to this many pixels of rising noise, before every other particle,
    /// where the host bound a scene-colour copy (any HDR frame). `0` is off.
    #[serde(default)]
    pub heat_haze: f32,
    /// Ribbon look (v7), read by [`RenderMode::Ribbon`] only.
    #[serde(default)]
    pub ribbon: RibbonDef,
    /// The mesh a [`RenderMode::Mesh`] particle draws (v8): a name registered with
    /// [`crate::scene::VfxLibrary::add_mesh`] (the registration path of the
    /// [`ShapeDef::MeshSurface`] shapes). `None` draws the built-in unit cube, like v7. A name with
    /// nothing registered (or a mesh the GPU refuses) draws nothing and is reported
    /// (`missing_mesh` / `bad_mesh`). Only valid with [`RenderMode::Mesh`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<String>,
}

/// How a [`RenderMode::Ribbon`] emitter's particles become strips (v7).
///
/// Ribbon particles are simulated on the CPU (gravity, drag, wind, vortex, attractor,
/// `kill_below` and the kill volume; not curl noise or depth collision, which validation rejects
/// on a ribbon) and drawn through the trail pipelines: a camera-facing strip through the
/// particles in spawn order, newest at the head (`t = 0`), oldest at the tail (`t = 1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RibbonDef {
    /// Which particles share a strip.
    #[serde(default)]
    pub link: RibbonLink,
    /// Points one strip keeps, `2..=`[`MAX_RIBBON_POINTS`]. An emitter-linked strip drops the
    /// oldest point past this. A spawn-group strip keeps the first `max_points` jobs of the
    /// frame (the newest jobs of a long frame are not added) and the track holds up to
    /// `MAX_GROUP_TRACK_POINTS` (4096), not one ring of `max_points`. A strip of `n` points
    /// draws `6 (n - 1)` vertices.
    #[serde(default = "default_ribbon_points")]
    pub max_points: u32,
    /// Width over length: multiplies the particle's current size (the full strip width), keyed
    /// by `t` from head (0) to tail (1). Empty is 1.
    #[serde(default)]
    pub width: Vec<FloatKey>,
    /// Colour over length: multiplies the particle colour, keyed like [`Self::width`] (the key
    /// alpha multiplies the alpha too). Empty is white.
    #[serde(default)]
    pub color: Vec<ColorKey>,
    /// How the sprite runs along the strip.
    #[serde(default)]
    pub texture: RibbonTexture,
}

impl Default for RibbonDef {
    fn default() -> Self {
        Self {
            link: RibbonLink::default(),
            max_points: default_ribbon_points(),
            width: Vec::new(),
            color: Vec::new(),
            texture: RibbonTexture::default(),
        }
    }
}

/// Which particles a ribbon strip links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RibbonLink {
    /// One strip per emitter of an instance: every particle in spawn order (a trail behind a
    /// moving source).
    #[default]
    Emitter,
    /// One strip per spawn group: the particles one frame spawned (a burst's arc, a ring).
    SpawnGroup,
}

/// How a ribbon's texture coordinate runs along it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum RibbonTexture {
    /// No sprite: the plain soft-edged strip.
    #[default]
    None,
    /// The sprite stretches once over the whole strip.
    Stretch,
    /// The sprite repeats every this many metres along the strip (`> 0`).
    Tile(f32),
}

impl Default for RenderModule {
    fn default() -> Self {
        Self {
            mode: RenderMode::Billboard,
            sprite: SpriteId::default(),
            flipbook: None,
            blend: BlendMode::default(),
            softness: 0.0,
            alpha_scale: 1.0,
            stretch: StretchDef::default(),
            light: None,
            emissive: default_one(),
            lit: false,
            self_shadow: default_self_shadow(),
            screen_floor_px: 0.0,
            heat_haze: 0.0,
            ribbon: RibbonDef::default(),
            mesh: None,
        }
    }
}

/// How each particle is turned into geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderMode {
    /// Camera-facing quad.
    Billboard,
    /// Camera-facing quad stretched along the particle's velocity.
    Stretched,
    /// An oriented mesh scaled by particle size (debris shards, not physics-driven): the built-in
    /// unit cube, or the registered mesh asset named by [`RenderModule::mesh`] (v8).
    Mesh,
    /// Camera-facing strips through the particles (v7, [`RenderModule::ribbon`]).
    Ribbon,
}

/// Procedural sprite tiles baked into the built-in atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SpriteId {
    /// Radial falloff, soft edge (smoke, glows).
    #[default]
    SoftDisc,
    /// Hard-edged disc.
    HardDisc,
    /// Ring (shockwaves).
    Ring,
    /// Horizontal streak (sparks, tracers).
    Spark,
    /// Turbulent puff (dust, smoke).
    Puff,
    /// Flare with four spikes (muzzle flash, glints).
    Flare,
    /// Faded square (mesh-like shards, confetti).
    Shard,
    /// First tile of the 16-frame puff flipbook (tiles 8..24 of the built-in atlas).
    PuffAnim,
    /// First tile of the 8-frame flame flipbook (tiles 24..32 of the built-in atlas).
    FireAnim,
    /// First tile of the 8-frame fireball-body flipbook (tiles 32..40, v4): a whole billowing
    /// fireball per frame with a baked temperature ramp (white-yellow core, orange body, sooty
    /// rim). A few large overlapping cards read as one fire body. Tint it with a neutral grey.
    FireballAnim,
}

/// Flipbook animation across atlas tiles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlipbookDef {
    /// Tiles per atlas row.
    pub cols: u32,
    /// Tiles per atlas column.
    pub rows: u32,
    /// Total animated tiles (row-major from the sprite tile).
    pub frames: u32,
    /// Playback model.
    pub mode: FlipbookMode,
    /// Each particle starts at a random frame (deterministic per emitter RNG).
    #[serde(default)]
    pub random_start: bool,
    /// Cross-fade from the current frame to the next. Off holds the current frame.
    #[serde(default)]
    pub blend_frames: bool,
}

/// How flipbook frames advance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FlipbookMode {
    /// Constant frames per second.
    Fps(f32),
    /// Spread the frames evenly over the particle lifetime.
    OverLife,
}

/// Blend mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BlendMode {
    /// `src + dst` (fire, sparks, tracers).
    #[default]
    Additive,
    /// Premultiplied-alpha over.
    Alpha,
}

/// Stretched-billboard tuning.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StretchDef {
    /// Quad length = `velocity_scale * |velocity|` (world metres).
    #[serde(default = "default_stretch_velocity")]
    pub velocity_scale: f32,
    /// Minimum length in metres so slow particles stay visible. A negative value is a multiple
    /// of the particle's current size instead (`-1.0`: a puff that is round at rest and
    /// streaks along its velocity when fast).
    #[serde(default = "default_stretch_min")]
    pub min_length: f32,
}

impl Default for StretchDef {
    fn default() -> Self {
        Self {
            velocity_scale: default_stretch_velocity(),
            min_length: default_stretch_min(),
        }
    }
}

/// Light emitted by particles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticleLightDef {
    /// sRGB hex colour.
    pub color: String,
    /// Scale on the linear colour. The gathered colour is this times the fire envelope times
    /// `min(instance intensity, 1)`. The renderer emits [`crate::PARTICLE_LIGHT_LUMENS_PER_UNIT`]
    /// lumens per unit of the brightest channel, times `fire_light_scale`. Not candela.
    pub intensity: f32,
    /// Radius of influence in metres.
    pub radius: f32,
}

fn default_one() -> f32 {
    1.0
}
fn default_stretch_velocity() -> f32 {
    0.05
}
fn default_stretch_min() -> f32 {
    0.02
}
fn default_self_shadow() -> f32 {
    0.65
}
fn default_ribbon_points() -> u32 {
    64
}
fn default_skinned_per_frame() -> u32 {
    256
}

// ---------------------------------------------------------------------------
// Validation + IO
// ---------------------------------------------------------------------------

impl VfxSystemDef {
    /// Parses a `.vfx.ron` document, checking the version and validating semantics.
    ///
    /// The `version` field is peeked before the full deserialise, so a *newer* version is rejected
    /// even when its body no longer matches the current struct.
    pub fn from_ron(text: &str) -> Result<Self, VfxFormatError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| VfxFormatError::Invalid(format!("cannot read version: {e}")))?;
        if version > VFX_FORMAT_VERSION {
            return Err(VfxFormatError::Version {
                found: version,
                expected: VFX_FORMAT_VERSION,
            });
        }
        let mut def: Self = ron::from_str(text)?;
        def.migrate_to_latest()?;
        def.validate()?;
        Ok(def)
    }

    /// Reject a newer version, or run the registered migrator chain to bring an older file up to
    /// [`VFX_FORMAT_VERSION`] (`docs/specs/12-assets.md` § 4a).
    pub fn migrate_to_latest(&mut self) -> Result<u32, VfxFormatError> {
        if self.version > VFX_FORMAT_VERSION {
            return Err(VfxFormatError::Version {
                found: self.version,
                expected: VFX_FORMAT_VERSION,
            });
        }
        if self.version < VFX_FORMAT_VERSION {
            let to = version::migrate(self, self.version, builtin_migrations(), VFX_FORMAT_VERSION)
                .map_err(|e| VfxFormatError::Migration(e.to_string()))?;
            self.version = to;
        }
        Ok(self.version)
    }

    /// Serialises back to a RON document with a trailing newline (round-trips through
    /// [`VfxSystemDef::from_ron`]).
    pub fn to_ron(&self) -> Result<String, VfxFormatError> {
        let mut s = ron::ser::to_string_pretty(self, regia_core::ron_pretty())
            .map_err(|e| VfxFormatError::Invalid(format!("serialize failed: {e}")))?;
        s.push('\n');
        Ok(s)
    }

    /// Semantic validation: range ordering, LOD array shapes, unique emitter names, hex colours,
    /// non-empty emitters, finite numbers.
    pub fn validate(&self) -> Result<(), VfxFormatError> {
        let invalid = |msg: &str| Err(VfxFormatError::Invalid(msg.to_string()));
        if self.name.is_empty() {
            return invalid("system name must not be empty");
        }
        if self.emitters.is_empty() {
            return invalid("a vfx system needs at least one emitter");
        }
        if let Some(bounds) = &self.system.bounds
            && !bounds.is_valid()
        {
            return invalid("system bounds must be finite with min <= max on every axis");
        }
        let mut names = std::collections::BTreeSet::new();
        for e in &self.emitters {
            if !names.insert(e.name.as_str()) {
                return invalid(&format!("duplicate emitter name '{}'", e.name));
            }
            if !e.spawn.lifetime.min.is_finite()
                || !e.spawn.lifetime.max.is_finite()
                || e.spawn.lifetime.min <= 0.0
                || e.spawn.lifetime.max < e.spawn.lifetime.min
            {
                return invalid(&format!("emitter '{}': bad lifetime range", e.name));
            }
            // Non-finite numbers from a hostile or hand-edited file are refused like a negative
            // rate: a NaN emissive or size reaches the GPU as NaN pixels. Regression:
            // tests/tqs_sweep.rs::vfx_validate_refuses_non_finite_numbers.
            match e.spawn.mode {
                SpawnMode::Rate(r) if !(r.is_finite() && r >= 0.0) => {
                    return invalid(&format!("emitter '{}': rate must be finite and >= 0", e.name));
                }
                SpawnMode::PerDistance { every } if !(every.is_finite() && every > 0.0) => {
                    return invalid(&format!("emitter '{}': 'every' must be finite and > 0", e.name));
                }
                _ => {}
            }
            if !(e.delay.is_finite() && e.delay >= 0.0) {
                return invalid(&format!("emitter '{}': delay must be finite and >= 0", e.name));
            }
            if e.duration.is_some_and(|d| !(d.is_finite() && d >= 0.0)) {
                return invalid(&format!("emitter '{}': duration must be finite and >= 0", e.name));
            }
            if !e.update.wind_response.is_finite() || !(0.0..=1.0).contains(&e.update.wind_response) {
                return invalid(&format!("emitter '{}': wind_response must be in 0..=1", e.name));
            }
            if !e.update.wind_drag.is_finite() || e.update.wind_drag < 0.0 {
                return invalid(&format!("emitter '{}': wind_drag must be >= 0", e.name));
            }
            // The finite-number set is closed over every float field of the format (ADR-2077;
            // deep-zai-vfx finding 1): forces, curve keys, shape tuples and lights are refused
            // non-finite like a rate, and scale-like fields also refuse negatives.
            let name = e.name.as_str();
            validate_non_negative(name, "count_scale", e.spawn.count_scale)?;
            validate_finite3(name, "gravity", e.update.gravity)?;
            validate_non_negative(name, "drag", e.update.drag)?;
            if let Some((amount, frequency, speed)) = e.update.curl {
                validate_finite(name, "curl amount", amount)?;
                validate_finite(name, "curl frequency", frequency)?;
                validate_finite(name, "curl speed", speed)?;
            }
            if let Some(vortex) = &e.update.vortex {
                validate_finite3(name, "vortex axis", vortex.axis)?;
                validate_finite(name, "vortex strength", vortex.strength)?;
                validate_finite(name, "vortex radial", vortex.radial)?;
            }
            if let Some(attractor) = &e.update.attractor {
                validate_finite3(name, "attractor position", attractor.position)?;
                validate_finite(name, "attractor strength", attractor.strength)?;
            }
            if let Some(plane) = e.update.kill_below {
                validate_finite(name, "kill_below", plane)?;
            }
            if let Some(collision) = &e.update.depth_collision {
                validate_finite(name, "depth_collision offset", collision.offset)?;
            }
            validate_curve_keys(name, "size_over_life", &e.update.size_over_life)?;
            validate_curve_keys(name, "alpha_over_life", &e.update.alpha_over_life)?;
            validate_curve_keys(name, "rotation_over_life", &e.update.rotation_over_life)?;
            match &e.init.shape {
                ShapeDef::Sphere { radius, .. } => validate_range("sphere radius", *radius)?,
                ShapeDef::Box { half_extents, .. } => {
                    validate_finite3(name, "box half_extents", *half_extents)?;
                }
                ShapeDef::Cone { length, .. } => validate_range("cone length", *length)?,
                _ => {}
            }
            match &e.init.velocity.mode {
                VelocityMode::Radial { speed } => validate_range("radial speed", *speed)?,
                VelocityMode::Directional { dir, speed, .. } => {
                    validate_finite3(name, "directional dir", *dir)?;
                    validate_range("directional speed", *speed)?;
                }
                _ => {}
            }
            validate_finite(name, "velocity inherit", e.init.velocity.inherit)?;
            validate_non_negative(name, "alpha_scale", e.render.alpha_scale)?;
            validate_non_negative(name, "softness", e.render.softness)?;
            validate_non_negative(name, "stretch velocity_scale", e.render.stretch.velocity_scale)?;
            validate_finite(name, "stretch min_length", e.render.stretch.min_length)?;
            validate_non_negative(name, "heat_haze", e.render.heat_haze)?;
            if let Some(flip) = &e.render.flipbook
                && let FlipbookMode::Fps(fps) = flip.mode
            {
                validate_non_negative(name, "flipbook fps", fps)?;
            }
            if let Some(light) = &e.render.light {
                validate_non_negative(name, "light intensity", light.intensity)?;
                validate_non_negative(name, "light radius", light.radius)?;
            }
            // The ribbon module round-trips whatever the render mode is, so its keys are checked
            // for every emitter.
            validate_curve_keys(name, "ribbon width", &e.render.ribbon.width)?;
            for key in &e.render.ribbon.color {
                validate_finite(name, "ribbon color time", key.t)?;
            }
            if let VelocityMode::Planar { speed, lift } = e.init.velocity.mode {
                validate_range("planar speed", speed)?;
                validate_range("planar lift", lift)?;
            }
            if let ShapeDef::Cone { angle, .. } = e.init.shape
                && !(0.0..89.0).contains(&angle)
            {
                return invalid(&format!(
                    "emitter '{}': cone angle must be in 0..89 degrees",
                    e.name
                ));
            }
            if let VelocityMode::Directional { spread, .. } = e.init.velocity.mode
                && !(0.0..=180.0).contains(&spread)
            {
                return invalid(&format!(
                    "emitter '{}': directional spread must be in 0..=180 degrees",
                    e.name
                ));
            }
            for (what, keys) in [
                ("size_over_life", &e.update.size_over_life),
                ("alpha_over_life", &e.update.alpha_over_life),
                ("rotation_over_life", &e.update.rotation_over_life),
            ] {
                if keys
                    .windows(2)
                    .any(|w| w[0].t.partial_cmp(&w[1].t).is_none_or(|o| o.is_gt()))
                {
                    return invalid(&format!(
                        "emitter '{}': {what} keys must be in increasing time order",
                        e.name
                    ));
                }
            }
            for (what, keys) in [
                ("init color", &e.init.color),
                ("color_over_life", &e.update.color_over_life),
            ] {
                if keys
                    .windows(2)
                    .any(|w| w[0].t.partial_cmp(&w[1].t).is_none_or(|o| o.is_gt()))
                {
                    return invalid(&format!(
                        "emitter '{}': {what} keys must be in increasing time order",
                        e.name
                    ));
                }
            }
            validate_range("size", e.init.size)?;
            validate_range("rotation", e.init.rotation)?;
            validate_range("spin", e.init.spin)?;
            if let Some(lod) = &self.system.lod {
                if lod.rate_scale.len() != lod.distances.len() + 1 {
                    return invalid("lod.rate_scale needs distances.len() + 1 entries");
                }
                if lod.distances.windows(2).any(|w| w[0] >= w[1]) {
                    return invalid("lod.distances must be strictly increasing");
                }
                if !lod.distances.iter().all(|d| d.is_finite()) {
                    return invalid("lod.distances must be finite");
                }
                if !lod.rate_scale.iter().all(|k| k.is_finite() && *k >= 0.0) {
                    return invalid("lod.rate_scale entries must be finite and >= 0");
                }
            }
            for key in &e.init.color {
                validate_finite(name, "init color time", key.t)?;
                parse_color(&key.color)
                    .map_err(|err| VfxFormatError::Invalid(format!("emitter '{}': {err}", e.name)))?;
            }
            for key in &e.update.color_over_life {
                validate_finite(name, "color_over_life time", key.t)?;
                parse_color(&key.color)
                    .map_err(|err| VfxFormatError::Invalid(format!("emitter '{}': {err}", e.name)))?;
            }
            if !(e.render.emissive.is_finite() && e.render.emissive >= 0.0) {
                return invalid(&format!("emitter '{}': emissive must be finite and >= 0", e.name));
            }
            if !(0.0..=1.0).contains(&e.render.self_shadow) {
                return invalid(&format!("emitter '{}': self_shadow must be in 0..=1", e.name));
            }
            if !(e.render.screen_floor_px.is_finite() && e.render.screen_floor_px >= 0.0) {
                return invalid(&format!(
                    "emitter '{}': screen_floor_px must be finite and >= 0",
                    e.name
                ));
            }
            if let Some(flip) = e.render.flipbook {
                if flip.cols == 0 || flip.rows == 0 || flip.frames == 0 {
                    return invalid(&format!("emitter '{}': flipbook counts must be > 0", e.name));
                }
                if flip.frames > flip.cols.saturating_mul(flip.rows) {
                    return invalid(&format!(
                        "emitter '{}': flipbook frames exceed cols * rows",
                        e.name
                    ));
                }
            }
            validate_v7(e)?;
        }
        self.validate_events()
    }

    /// The rules of event links and of [`SpawnMode::Event`] emitters: in one pass over the links, so
    /// a hostile file with many emitters costs linear time, and with a depth limit, so walking a
    /// valid graph ([`Self::event_tail`]) stays small.
    fn validate_events(&self) -> Result<(), VfxFormatError> {
        let invalid =
            |name: &str, msg: String| Err(VfxFormatError::Invalid(format!("emitter '{name}': {msg}")));
        let index: std::collections::HashMap<&str, usize> = self
            .emitters
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.as_str(), i))
            .collect();
        // The largest LOD rate scale (1 without LOD): the band the instance sits in multiplies a
        // link's child count at runtime.
        let lod_max = f64::from(
            self.system
                .lod
                .as_ref()
                .map_or(1.0, |lod| lod.rate_scale.iter().copied().fold(1.0, f32::max)),
        );
        let mut parents = vec![0u32; self.emitters.len()];
        for e in &self.emitters {
            if e.update.events.len() > MAX_EVENTS_PER_EMITTER {
                return invalid(&e.name, format!("more than {MAX_EVENTS_PER_EMITTER} events"));
            }
            for event in &e.update.events {
                let Some(&child) = index.get(event.child.as_str()) else {
                    return invalid(
                        &e.name,
                        format!("event child '{}' is not in this system", event.child),
                    );
                };
                if self.emitters[child].spawn.mode != SpawnMode::Event {
                    return invalid(
                        &e.name,
                        format!("event child '{}' must use spawn mode Event", event.child),
                    );
                }
                if event.count == 0 || event.count > MAX_EVENT_COUNT {
                    return invalid(&e.name, format!("event count must be in 1..={MAX_EVENT_COUNT}"));
                }
                // The packed link field carries the scaled count in 24 bits, so a worst case
                // above it wraps on the GPU (exactly 2^24 packs to zero and the link never
                // fires). Refuse the authored worst case: count x the child's count_scale x the
                // largest LOD rate scale x the intensity cap (deep-zai-vfx finding 2).
                // `events::scaled_count` clamps whatever reaches it another way.
                let worst = f64::from(event.count)
                    * f64::from(self.emitters[child].spawn.count_scale)
                    * lod_max
                    * f64::from(MAX_INSTANCE_INTENSITY);
                if worst > f64::from(MAX_PACKED_EVENT_COUNT) {
                    return invalid(
                        &e.name,
                        format!(
                            "event link to '{}' scales to {worst} children at worst, above the packed limit {}",
                            event.child, MAX_PACKED_EVENT_COUNT
                        ),
                    );
                }
                if !event.probability.is_finite() || !(0.0..=1.0).contains(&event.probability) {
                    return invalid(&e.name, "event probability must be in 0..=1".to_string());
                }
                if !event.inherit_velocity.is_finite() || !(0.0..=1.0).contains(&event.inherit_velocity) {
                    return invalid(&e.name, "event inherit_velocity must be in 0..=1".to_string());
                }
                parents[child] += 1;
            }
        }
        for (e, &count) in self.emitters.iter().zip(&parents) {
            if e.spawn.mode != SpawnMode::Event {
                continue;
            }
            if e.delay != 0.0 || e.duration.is_some() {
                return invalid(&e.name, "spawn mode Event has no delay or duration".to_string());
            }
            if e.render.light.is_some() {
                return invalid(
                    &e.name,
                    "spawn mode Event cannot have a light (its position is not known on the CPU)".to_string(),
                );
            }
            if count == 0 {
                return invalid(
                    &e.name,
                    "spawn mode Event needs a parent event that names it".to_string(),
                );
            }
        }
        // Topological walk (Kahn 1962): what it never reaches is on a cycle; one particle could
        // fire without end. Depth is the longest chain of links above an emitter.
        let mut depth = vec![0usize; self.emitters.len()];
        let mut ready: Vec<usize> = parents
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count == 0)
            .map(|(i, _)| i)
            .collect();
        let mut walked = 0;
        while let Some(at) = ready.pop() {
            walked += 1;
            if depth[at] > MAX_EVENT_DEPTH {
                return invalid(
                    &self.emitters[at].name,
                    format!("event links are nested more than {MAX_EVENT_DEPTH} deep"),
                );
            }
            for event in &self.emitters[at].update.events {
                if let Some(&child) = index.get(event.child.as_str()) {
                    depth[child] = depth[child].max(depth[at] + 1);
                    parents[child] -= 1;
                    if parents[child] == 0 {
                        ready.push(child);
                    }
                }
            }
        }
        if walked < self.emitters.len() {
            let stuck = parents.iter().position(|&count| count > 0).unwrap_or(0);
            return invalid(&self.emitters[stuck].name, "event links form a cycle".to_string());
        }
        Ok(())
    }

    /// Index of the emitter called `name`.
    #[must_use]
    pub fn emitter_index(&self, name: &str) -> Option<usize> {
        self.emitters.iter().position(|e| e.name == name)
    }

    /// Whether any emitter fires event spawns.
    #[must_use]
    pub fn has_events(&self) -> bool {
        self.emitters.iter().any(|e| !e.update.events.is_empty())
    }

    /// Seconds after a particle of emitter `index` dies that the children it fired can still be
    /// alive: the longest child lifetime along its event links. `0` without events.
    #[must_use]
    pub fn event_tail(&self, index: usize) -> f32 {
        self.event_tail_at(index, 0)
    }

    fn event_tail_at(&self, index: usize, depth: usize) -> f32 {
        let Some(emitter) = self.emitters.get(index) else {
            return 0.0;
        };
        // The bound keeps an unvalidated cycle from recursing forever.
        if depth > MAX_EVENT_DEPTH {
            return 0.0;
        }
        emitter
            .update
            .events
            .iter()
            .filter_map(|event| {
                let child = self.emitter_index(&event.child)?;
                let life = self.emitters[child].spawn.lifetime.max;
                Some(life + self.event_tail_at(child, depth + 1))
            })
            .fold(0.0, f32::max)
    }

    /// Distance-LOD rate scale for a viewer `distance` (1.0 when no LOD).
    pub fn lod_scale(&self, distance: f32) -> f32 {
        match &self.system.lod {
            None => 1.0,
            Some(lod) => {
                let band = lod.distances.iter().filter(|&&d| d <= distance).count();
                lod.rate_scale
                    .get(band)
                    .copied()
                    .unwrap_or_else(|| *lod.rate_scale.last().unwrap_or(&1.0))
            }
        }
    }
}

/// v1 bodies already deserialise: every v2 field has a serde default. The step stamps the version.
fn migrate_v1_to_v2(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// v2 bodies already deserialise. v3 only adds the `FireAnim` sprite, which old files do not use.
fn migrate_v2_to_v3(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// v3 bodies already deserialise. v4 adds the `FireballAnim` sprite and `heat_haze` (default
/// off), which old files do not use.
fn migrate_v3_to_v4(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// v4 bodies already deserialise. v5 adds the `Planar` velocity mode, which old files do not
/// use; their `Radial` velocities keep their meaning.
fn migrate_v4_to_v5(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// v6 bodies already deserialise. v7 adds the mesh shapes, the `Normal` velocity, `kill_volume`
/// (none), the `Ribbon` render mode and `ribbon` (ignored by the other modes), which old files do
/// not use, so a v6 effect simulates bit-identically.
fn migrate_v6_to_v7(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// v7 bodies already deserialise. v8 adds `render.mesh` (none: a `Mesh` particle keeps the
/// built-in cube), so a v7 effect draws and simulates bit-identically (ADR-7210).
fn migrate_v7_to_v8(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// v5 bodies already deserialise. v6 adds `wind_response` (default 0: the wind does not touch the
/// particles), `wind_drag` (ignored at response 0), `events` (none) and the `Event` spawn mode,
/// which old files do not use, so a v5 effect simulates bit-identically.
fn migrate_v5_to_v6(def: &mut VfxSystemDef) -> Result<(), String> {
    let _ = def;
    Ok(())
}

/// The built-in migration chain for `.vfx.ron`. v1 files load as v6 with the default quality
/// fields (emissive 1, unlit, no flipbook phase). v2 and v3 files load with the same sprites
/// and no heat haze; v4 files load unchanged (v5 only adds the `Planar` velocity mode); v5 files
/// load with no wind response and no events; v6 files load with no kill volume and no ribbon.
/// `regia upgrade-project` links this crate and re-saves old files through the loader.
fn builtin_migrations() -> &'static [VersionStep<VfxSystemDef>] {
    &[
        VersionStep {
            from: 1,
            to: 2,
            migrate: migrate_v1_to_v2,
        },
        VersionStep {
            from: 2,
            to: 3,
            migrate: migrate_v2_to_v3,
        },
        VersionStep {
            from: 3,
            to: 4,
            migrate: migrate_v3_to_v4,
        },
        VersionStep {
            from: 4,
            to: 5,
            migrate: migrate_v4_to_v5,
        },
        VersionStep {
            from: 5,
            to: 6,
            migrate: migrate_v5_to_v6,
        },
        VersionStep {
            from: 6,
            to: 7,
            migrate: migrate_v6_to_v7,
        },
        VersionStep {
            from: 7,
            to: 8,
            migrate: migrate_v7_to_v8,
        },
    ]
}

/// The versions a built-in migration step leaves (`from` side), for `cargo xtask check-formats`
/// to verify a version bump ships with a migrator from the previous version.
pub fn vfx_migration_sources() -> Vec<u32> {
    builtin_migrations().iter().map(|s| s.from).collect()
}

/// The serde field-list fingerprint of the `.vfx.ron` format (hashed by `cargo xtask check-formats`).
///
/// The root struct *and* every nested struct are listed, so a shape change anywhere in the tree
/// changes the hash.
pub fn vfx_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<VfxSystemDef>(),
        regia_core::schema::serde_field_names::<SystemModule>(),
        regia_core::schema::serde_field_names::<LodDef>(),
        regia_core::schema::serde_field_names::<EmitterDef>(),
        regia_core::schema::serde_field_names::<SpawnModule>(),
        regia_core::schema::serde_field_names::<InitModule>(),
        regia_core::schema::serde_field_names::<VelocityDef>(),
        regia_core::schema::serde_field_names::<UpdateModule>(),
        regia_core::schema::serde_field_names::<VortexDef>(),
        regia_core::schema::serde_field_names::<AttractorDef>(),
        regia_core::schema::serde_field_names::<DepthCollisionDef>(),
        regia_core::schema::serde_field_names::<RenderModule>(),
        regia_core::schema::serde_field_names::<FlipbookDef>(),
        regia_core::schema::serde_field_names::<StretchDef>(),
        regia_core::schema::serde_field_names::<ParticleLightDef>(),
        regia_core::schema::serde_field_names::<RangeDef>(),
        regia_core::schema::serde_field_names::<FloatKey>(),
        regia_core::schema::serde_field_names::<ColorKey>(),
        regia_core::schema::serde_field_names::<EventSpawnDef>(),
        regia_core::schema::serde_field_names::<KillVolumeDef>(),
        regia_core::schema::serde_field_names::<RibbonDef>(),
        regia_core::schema::serde_field_names::<FixedBoundsDef>(),
        regia_core::schema::serde_variant_names::<EventTrigger>(),
        regia_core::schema::serde_variant_names::<KillShape>(),
        regia_core::schema::serde_variant_names::<KillSide>(),
        regia_core::schema::serde_variant_names::<RibbonLink>(),
        regia_core::schema::serde_variant_names::<RibbonTexture>(),
        regia_core::schema::serde_variant_names::<SpawnMode>(),
        regia_core::schema::serde_variant_names::<ShapeDef>(),
        regia_core::schema::serde_variant_names::<VelocityMode>(),
        regia_core::schema::serde_variant_names::<RenderMode>(),
        regia_core::schema::serde_variant_names::<SpriteId>(),
        regia_core::schema::serde_variant_names::<FlipbookMode>(),
        regia_core::schema::serde_variant_names::<BlendMode>(),
    ])
}

/// The v7 rules of one emitter: mesh shapes, the kill volume and ribbons.
fn validate_v7(e: &EmitterDef) -> Result<(), VfxFormatError> {
    let invalid = |msg: String| Err(VfxFormatError::Invalid(format!("emitter '{}': {msg}", e.name)));
    let finite3 = |v: (f32, f32, f32)| v.0.is_finite() && v.1.is_finite() && v.2.is_finite();
    match &e.init.shape {
        ShapeDef::MeshSurface { mesh } | ShapeDef::SkinnedMesh { mesh, .. } if mesh.is_empty() => {
            return invalid("a mesh shape needs a mesh name".into());
        }
        ShapeDef::SkinnedMesh { max_per_frame, .. }
            if *max_per_frame == 0 || *max_per_frame > MAX_SKINNED_SPAWNS_PER_FRAME =>
        {
            return invalid(format!(
                "max_per_frame must be in 1..={MAX_SKINNED_SPAWNS_PER_FRAME}"
            ));
        }
        _ => {}
    }
    if let VelocityMode::Normal { speed } = e.init.velocity.mode {
        validate_range("normal speed", speed)?;
    }
    if e.spawn.mode == SpawnMode::Event {
        // Children start on the GPU (`child_emit`), which knows the analytic shapes only.
        if e.init.shape.mesh_name().is_some() {
            return invalid("spawn mode Event cannot use a mesh shape".into());
        }
        if matches!(e.init.velocity.mode, VelocityMode::Normal { .. }) {
            return invalid("spawn mode Event cannot use the Normal velocity".into());
        }
    }
    if let Some(volume) = &e.update.kill_volume {
        if !finite3(volume.center) {
            return invalid("kill_volume center must be finite".into());
        }
        let ok = match volume.shape {
            KillShape::Box { half_extents: h } => finite3(h) && h.0 > 0.0 && h.1 > 0.0 && h.2 > 0.0,
            KillShape::Sphere { radius } => radius.is_finite() && radius > 0.0,
        };
        if !ok {
            return invalid("kill_volume extents must be finite and > 0".into());
        }
    }
    // v8: `render.mesh` names a registered mesh and belongs to `Mesh` particles only.
    match e.render.mesh.as_deref() {
        Some("") => return invalid("render mesh must not be empty (omit it for the built-in cube)".into()),
        Some(_) if e.render.mode != RenderMode::Mesh => {
            return invalid("render mesh needs mode Mesh".into());
        }
        _ => {}
    }
    if e.render.mode == RenderMode::Ribbon {
        let r = &e.render.ribbon;
        if r.max_points < 2 || r.max_points > MAX_RIBBON_POINTS {
            return invalid(format!("ribbon max_points must be in 2..={MAX_RIBBON_POINTS}"));
        }
        if let RibbonTexture::Tile(length) = r.texture
            && !(length.is_finite() && length > 0.0)
        {
            return invalid("ribbon Tile length must be finite and > 0".into());
        }
        for key in &r.color {
            parse_color(&key.color)
                .map_err(|err| VfxFormatError::Invalid(format!("emitter '{}': {err}", e.name)))?;
        }
        // Ribbon particles live on the CPU: nothing the GPU alone knows may drive them.
        if e.spawn.mode == SpawnMode::Event || !e.update.events.is_empty() {
            return invalid("a Ribbon emitter cannot fire or be an event spawn".into());
        }
        if e.update.curl.is_some() || e.update.depth_collision.is_some() {
            return invalid("a Ribbon emitter cannot use curl noise or depth collision".into());
        }
        if e.render.heat_haze > 0.0 || e.render.light.is_some() {
            return invalid("a Ribbon emitter cannot have heat haze or a light".into());
        }
    }
    Ok(())
}

fn validate_range(name: &str, r: RangeDef) -> Result<(), VfxFormatError> {
    // Regression: tests/tqs_sweep.rs::vfx_validate_refuses_non_finite_numbers.
    if !(r.min.is_finite() && r.max.is_finite()) {
        Err(VfxFormatError::Invalid(format!("range '{name}' is not finite")))
    } else if r.max < r.min {
        Err(VfxFormatError::Invalid(format!("range '{name}' has max < min")))
    } else {
        Ok(())
    }
}

/// `v` is finite. The finite-number rule of ADR-2077, closed over every float field of the
/// format (deep-zai-vfx finding 1): a NaN or infinite force, scale, curve key or light reaches
/// the update kernel, the draw or the CPU sim as NaN positions and NaN pixels.
fn validate_finite(emitter: &str, what: &str, v: f32) -> Result<(), VfxFormatError> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(VfxFormatError::Invalid(format!(
            "emitter '{emitter}': {what} must be finite"
        )))
    }
}

/// `v` is finite and at least 0: a scale, width or rate the engine only grows with, where a
/// negative value is an authored mistake (negative drag amplifies without bound, a negative
/// alpha flips the blend).
fn validate_non_negative(emitter: &str, what: &str, v: f32) -> Result<(), VfxFormatError> {
    if v.is_finite() && v >= 0.0 {
        Ok(())
    } else {
        Err(VfxFormatError::Invalid(format!(
            "emitter '{emitter}': {what} must be finite and >= 0"
        )))
    }
}

/// Every component of a tuple field is finite (a direction, position or extent).
fn validate_finite3(emitter: &str, what: &str, v: (f32, f32, f32)) -> Result<(), VfxFormatError> {
    validate_finite(emitter, what, v.0)?;
    validate_finite(emitter, what, v.1)?;
    validate_finite(emitter, what, v.2)
}

/// Every key of a scalar curve has a finite time and value. The ordering rule above rejects a
/// `t` it can compare; a NaN `t` compares to nothing, so it slips through that check alone.
fn validate_curve_keys(emitter: &str, what: &str, keys: &[FloatKey]) -> Result<(), VfxFormatError> {
    for key in keys {
        validate_finite(emitter, &format!("{what} time"), key.t)?;
        validate_finite(emitter, &format!("{what} value"), key.v)?;
    }
    Ok(())
}

/// Parses `#rrggbb(aa)` into linear RGBA via [`regia_core::math::Color`].
pub(crate) fn parse_color(hex: &str) -> Result<[f32; 4], String> {
    regia_core::math::Color::from_hex(hex)
        .map(|c| c.to_vec4().to_array())
        .map_err(|e| format!("bad hex color '{hex}': {e}"))
}

/// Direction tuple → unit vector (zero stays zero).
pub(crate) fn dir_from_tuple(d: (f32, f32, f32)) -> Vec3 {
    let v = Vec3::new(d.0, d.1, d.2);
    if v.length_squared() < 1e-12 {
        Vec3::ZERO
    } else {
        v.normalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_always_an_explicit_struct() {
        // The format docs promise no bare-float shorthand: a range is `(min, max)`.
        let ok: RangeDef = ron::from_str("(min: 0.4, max: 0.4)").expect("explicit range parses");
        assert_eq!(ok, RangeDef::from(0.4));
        assert!(ron::from_str::<RangeDef>("0.4").is_err());
    }

    fn muzzle() -> VfxSystemDef {
        VfxSystemDef::from_ron(
            r##"
(
    version: 1,
    name: "muzzle_flash",
    system: (budget: 256),
    emitters: [
        (
            name: "flash",
            spawn: (mode: Burst(24), lifetime: (min: 0.05, max: 0.12)),
            init: (
                shape: Cone(angle: 12.0, length: (min: 0.2, max: 0.8)),
                velocity: (mode: Directional(dir: (0.0, 0.0, -1.0), spread: 8.0, speed: (min: 8.0, max: 20.0))),
                size: (min: 0.1, max: 0.4),
                color: [(t: 0.0, color: "#fff2c8")],
            ),
            update: (drag: 6.0),
            render: (
                mode: Billboard,
                sprite: Flare,
                blend: Additive,
                stretch: (velocity_scale: 0.02),
            ),
        ),
    ],
)
"##,
        )
        .expect("valid muzzle asset")
    }

    #[test]
    fn validate_rejects_bad_cone_spread_and_unsorted_keys() {
        let mut d = muzzle();
        assert!(d.validate().is_ok());
        let ok = d.clone();
        d.emitters[0].init.shape = ShapeDef::Cone {
            angle: 90.0,
            length: RangeDef { min: 0.2, max: 0.8 },
        };
        assert!(matches!(d.validate(), Err(VfxFormatError::Invalid(m)) if m.contains("cone angle")));
        let mut d = ok.clone();
        d.emitters[0].init.velocity.mode = VelocityMode::Directional {
            dir: (0.0, 0.0, 1.0),
            spread: 360.0,
            speed: RangeDef { min: 1.0, max: 2.0 },
        };
        assert!(matches!(d.validate(), Err(VfxFormatError::Invalid(m)) if m.contains("spread")));
        let mut d = ok.clone();
        d.emitters[0].update.alpha_over_life = vec![FloatKey { t: 1.0, v: 0.0 }, FloatKey { t: 0.0, v: 1.0 }];
        assert!(matches!(d.validate(), Err(VfxFormatError::Invalid(m)) if m.contains("alpha_over_life")));
        let mut d = ok;
        d.emitters[0].update.color_over_life = vec![
            ColorKey {
                t: 0.8,
                color: "#ffffff".into(),
            },
            ColorKey {
                t: 0.2,
                color: "#000000".into(),
            },
        ];
        assert!(matches!(d.validate(), Err(VfxFormatError::Invalid(m)) if m.contains("color_over_life")));
    }

    #[test]
    fn round_trips_through_ron() {
        let def = muzzle();
        let text = def.to_ron().expect("serialize");
        let back = VfxSystemDef::from_ron(&text).expect("re-parse");
        assert_eq!(def, back);
    }

    #[test]
    fn rejects_unknown_version() {
        // One past the current version (8 since ADR-7210) is newer than this reader.
        let newer = VFX_FORMAT_VERSION + 1;
        let err = VfxSystemDef::from_ron(&format!("(version: {newer}, name: \"x\", emitters: [])"))
            .expect_err("must be invalid");
        assert!(matches!(err, VfxFormatError::Version { found, .. } if found == newer));
    }

    #[test]
    fn v1_document_migrates_and_keeps_defaults() {
        let def = muzzle();
        assert_eq!(
            def.version, VFX_FORMAT_VERSION,
            "v1 fixture migrates to the current version"
        );
        let render = &def.emitters[0].render;
        assert!((render.emissive - 1.0).abs() < 1e-5);
        assert!(!render.lit);
        assert!(render.flipbook.is_none());
        assert_eq!(render.screen_floor_px, 0.0);
    }

    #[test]
    fn v3_document_migrates_to_v4_without_haze() {
        let text = r#"
(version: 3, name: "fire", emitters: [(
    name: "e", spawn: (mode: Burst(4), lifetime: (min: 0.5, max: 1.0)),
    init: (size: (min: 1.0, max: 1.0)),
    render: (mode: Billboard, sprite: FireAnim, blend: Alpha, emissive: 6.0),
)])"#;
        let def = VfxSystemDef::from_ron(text).expect("v3 loads");
        assert_eq!(def.version, VFX_FORMAT_VERSION);
        assert_eq!(def.emitters[0].render.sprite, SpriteId::FireAnim);
        assert_eq!(def.emitters[0].render.heat_haze, 0.0);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = VfxSystemDef::from_ron("(version: 1, name: \"x\", emitters: [], sparkle: true)")
            .expect_err("must be invalid");
        assert!(matches!(err, VfxFormatError::Parse(_)));
    }

    #[test]
    fn rejects_empty_emitters_and_bad_ranges() {
        assert!(
            VfxSystemDef::from_ron("(version: 1, name: \"x\", emitters: [])")
                .expect_err("must be invalid")
                .to_string()
                .contains("at least one emitter")
        );
        let text = r#"
(version: 1, name: "x", emitters: [(
    name: "e", spawn: (mode: Rate(1.0), lifetime: (min: 0.5, max: 0.1)),
    init: (size: (min: 1.0, max: 1.0)), render: (mode: Billboard),
)])"#;
        assert!(VfxSystemDef::from_ron(text).is_err());
    }

    #[test]
    fn rejects_non_finite_lifetimes() {
        for (min, max) in [
            (f32::INFINITY, f32::INFINITY),
            (0.5, f32::INFINITY),
            (f32::NAN, 1.0),
        ] {
            let mut def = muzzle();
            def.emitters[0].spawn.lifetime = RangeDef { min, max };
            assert!(def.validate().is_err(), "({min}, {max}) must be rejected");
        }
    }

    #[test]
    fn rejects_bad_lod_arrays() {
        let mut def = muzzle();
        def.system.lod = Some(LodDef {
            distances: vec![10.0, 20.0],
            rate_scale: vec![1.0, 0.5], // needs 3
        });
        assert!(def.validate().is_err());
        def.system.lod = Some(LodDef {
            distances: vec![10.0, 20.0],
            rate_scale: vec![1.0, 0.5, 0.2],
        });
        assert!(def.validate().is_ok());
        assert!((def.lod_scale(5.0) - 1.0).abs() < 1e-6);
        assert!((def.lod_scale(15.0) - 0.5).abs() < 1e-6);
        assert!((def.lod_scale(999.0) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_hex_colors() {
        let mut def = muzzle();
        def.emitters[0].init.color = vec![ColorKey {
            t: 0.0,
            color: "nope".into(),
        }];
        assert!(def.validate().is_err());
    }

    /// A burst parent with one death link to an event-only child.
    fn parent_child(event: EventSpawnDef) -> VfxSystemDef {
        let mut def = muzzle();
        let mut child = def.emitters[0].clone();
        child.name = "child".into();
        child.spawn.mode = SpawnMode::Event;
        def.emitters[0].update.events = vec![event];
        def.emitters.push(child);
        def
    }

    fn link() -> EventSpawnDef {
        EventSpawnDef {
            on: EventTrigger::Death,
            child: "child".into(),
            count: 3,
            probability: 1.0,
            inherit_velocity: 0.0,
        }
    }

    #[test]
    fn v5_document_loads_with_no_wind_and_no_events() {
        let text = r#"
(version: 5, name: "dust", emitters: [(
    name: "e", spawn: (mode: Rate(4.0), lifetime: (min: 0.5, max: 1.0)),
    init: (size: (min: 1.0, max: 1.0)),
    render: (mode: Billboard),
)])"#;
        let def = VfxSystemDef::from_ron(text).expect("v5 loads");
        assert_eq!(def.version, VFX_FORMAT_VERSION);
        let update = &def.emitters[0].update;
        assert_eq!(update.wind_response, 0.0);
        assert!(update.events.is_empty());
        assert!(!def.has_events());
        assert_eq!(vfx_migration_sources(), [1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn event_links_round_trip_and_report_their_tail() {
        let mut def = parent_child(link());
        def.emitters[0].update.wind_response = 0.5;
        def.emitters[0].update.wind_drag = 2.0;
        let back = VfxSystemDef::from_ron(&def.to_ron().expect("serialize")).expect("re-parse");
        assert_eq!(def, back);
        assert!(back.has_events());
        assert_eq!(back.event_tail(0), back.emitters[1].spawn.lifetime.max);
        assert_eq!(back.event_tail(1), 0.0);
    }

    #[test]
    fn bad_wind_values_are_rejected() {
        for (response, drag) in [
            (-0.1, 1.0),
            (1.1, 1.0),
            (f32::NAN, 1.0),
            (0.5, -1.0),
            (0.5, f32::INFINITY),
        ] {
            let mut def = muzzle();
            def.emitters[0].update.wind_response = response;
            def.emitters[0].update.wind_drag = drag;
            assert!(def.validate().is_err(), "({response}, {drag}) must be rejected");
        }
    }

    #[test]
    fn bad_event_links_are_rejected() {
        let reasons = |def: VfxSystemDef| def.validate().expect_err("must be invalid").to_string();
        let unknown = parent_child(EventSpawnDef {
            child: "nope".into(),
            ..link()
        });
        assert!(reasons(unknown).contains("not in this system"));
        for bad in [
            EventSpawnDef { count: 0, ..link() },
            EventSpawnDef {
                count: MAX_EVENT_COUNT + 1,
                ..link()
            },
            EventSpawnDef {
                probability: 1.5,
                ..link()
            },
            EventSpawnDef {
                inherit_velocity: f32::NAN,
                ..link()
            },
        ] {
            assert!(parent_child(bad).validate().is_err());
        }
        // A child that schedules itself.
        let mut scheduled = parent_child(link());
        scheduled.emitters[1].spawn.mode = SpawnMode::Burst(1);
        assert!(reasons(scheduled).contains("must use spawn mode Event"));
        // An event emitter nobody fires.
        let mut orphan = parent_child(link());
        orphan.emitters[0].update.events.clear();
        assert!(reasons(orphan).contains("needs a parent"));
        // Too many links on one emitter.
        let mut many = parent_child(link());
        many.emitters[0].update.events = vec![link(); MAX_EVENTS_PER_EMITTER + 1];
        assert!(reasons(many).contains("more than"));
        // An event emitter's timing and light.
        let mut timed = parent_child(link());
        timed.emitters[1].delay = 0.5;
        assert!(reasons(timed).contains("delay"));
    }

    #[test]
    fn event_cycles_are_rejected() {
        let mut def = parent_child(link());
        def.emitters[1].update.events = vec![EventSpawnDef {
            child: "child".into(),
            ..link()
        }];
        assert!(
            def.validate()
                .expect_err("self loop")
                .to_string()
                .contains("cycle"),
            "a child firing itself never ends"
        );
    }

    #[test]
    fn deep_event_chains_are_rejected_and_a_chain_at_the_limit_loads() {
        let chain = |links: usize| {
            let mut def = muzzle();
            let root = def.emitters[0].clone();
            let mut previous = 0;
            for level in 1..=links {
                let name = format!("level{level}");
                let mut child = root.clone();
                child.name = name.clone();
                child.spawn.mode = SpawnMode::Event;
                child.update.events.clear();
                def.emitters[previous].update.events = vec![EventSpawnDef {
                    child: name,
                    ..link()
                }];
                def.emitters.push(child);
                previous = level;
            }
            def
        };
        chain(MAX_EVENT_DEPTH).validate().expect("at the limit");
        assert!(
            chain(MAX_EVENT_DEPTH + 1)
                .validate()
                .expect_err("too deep")
                .to_string()
                .contains("nested")
        );
        // The tail adds up along the chain, whatever the graph's width.
        let tail = chain(3).event_tail(0);
        let life = chain(3).emitters[0].spawn.lifetime.max;
        assert!((tail - 3.0 * life).abs() < 1e-5, "{tail}");
    }

    #[test]
    fn v6_document_loads_with_no_kill_volume_and_no_ribbon() {
        let text = r#"
(version: 6, name: "dust", emitters: [(
    name: "e", spawn: (mode: Rate(4.0), lifetime: (min: 0.5, max: 1.0)),
    init: (size: (min: 1.0, max: 1.0)),
    update: (wind_response: 0.5),
    render: (mode: Billboard),
)])"#;
        let def = VfxSystemDef::from_ron(text).expect("v6 loads");
        assert_eq!(def.version, VFX_FORMAT_VERSION);
        assert!(def.emitters[0].update.kill_volume.is_none());
        assert_eq!(def.emitters[0].render.ribbon, RibbonDef::default());
        // The chain ends one step below the current version (v7 → v8 since ADR-7210).
        assert_eq!(
            *vfx_migration_sources().last().expect("steps"),
            VFX_FORMAT_VERSION - 1
        );
    }

    fn v7_text() -> &'static str {
        r##"
(version: 7, name: "v7", emitters: [
    (
        name: "skin", spawn: (mode: Rate(40.0), lifetime: (min: 0.5, max: 1.0)),
        init: (
            shape: SkinnedMesh(mesh: "meshes/hero.mesh", max_per_frame: 32),
            velocity: (mode: Normal(speed: (min: 0.5, max: 1.0))),
            size: (min: 0.1, max: 0.1),
        ),
        update: (kill_volume: Some((shape: Box(half_extents: (1.0, 2.0, 3.0)), center: (0.0, 1.0, 0.0), kill: Outside))),
        render: (mode: Billboard),
    ),
    (
        name: "trail", spawn: (mode: Rate(30.0), lifetime: (min: 1.0, max: 1.0)),
        init: (shape: MeshSurface(mesh: "meshes/rock.mesh"), size: (min: 0.2, max: 0.2)),
        update: (kill_volume: Some((shape: Sphere(radius: 4.0)))),
        render: (mode: Ribbon, blend: Alpha, ribbon: (
            link: SpawnGroup, max_points: 16,
            width: [(t: 0.0, v: 1.0), (t: 1.0, v: 0.0)],
            color: [(t: 0.0, color: "#ffffff"), (t: 1.0, color: "#ff000000")],
            texture: Tile(0.5),
        )),
    ),
])"##
    }

    #[test]
    fn v7_shapes_kill_volumes_and_ribbons_round_trip() {
        let def = VfxSystemDef::from_ron(v7_text()).expect("v7 parses");
        let back = VfxSystemDef::from_ron(&def.to_ron().expect("serialize")).expect("re-parse");
        assert_eq!(def, back);
        assert_eq!(def.emitters[0].init.shape.mesh_name(), Some("meshes/hero.mesh"));
        let volume = def.emitters[1].update.kill_volume.as_ref().expect("volume");
        assert_eq!(volume.kill, KillSide::Inside, "the default side");
        assert_eq!(def.emitters[1].render.ribbon.texture, RibbonTexture::Tile(0.5));
    }

    #[test]
    fn bad_v7_content_is_rejected() {
        let base = || VfxSystemDef::from_ron(v7_text()).expect("v7 parses");
        let reason = |def: VfxSystemDef| def.validate().expect_err("must be invalid").to_string();
        let mut d = base();
        d.emitters[1].init.shape = ShapeDef::MeshSurface { mesh: String::new() };
        assert!(reason(d).contains("mesh name"));
        let mut d = base();
        d.emitters[0].init.shape = ShapeDef::SkinnedMesh {
            mesh: "m".into(),
            max_per_frame: 0,
        };
        assert!(reason(d).contains("max_per_frame"));
        for shape in [
            KillShape::Sphere { radius: 0.0 },
            KillShape::Sphere { radius: f32::NAN },
            KillShape::Box {
                half_extents: (1.0, -1.0, 1.0),
            },
        ] {
            let mut d = base();
            d.emitters[1].update.kill_volume = Some(KillVolumeDef {
                shape,
                center: (0.0, 0.0, 0.0),
                kill: KillSide::Inside,
            });
            assert!(reason(d).contains("kill_volume"));
        }
        for points in [0, 1, MAX_RIBBON_POINTS + 1] {
            let mut d = base();
            d.emitters[1].render.ribbon.max_points = points;
            assert!(reason(d).contains("max_points"));
        }
        let mut d = base();
        d.emitters[1].render.ribbon.texture = RibbonTexture::Tile(0.0);
        assert!(reason(d).contains("Tile"));
        let mut d = base();
        d.emitters[1].update.curl = Some((1.0, 1.0, 1.0));
        assert!(reason(d).contains("curl"));
        // An event child keeps to what the GPU child kernel samples.
        let mut d = parent_child(link());
        d.emitters[1].init.shape = ShapeDef::MeshSurface { mesh: "m".into() };
        assert!(reason(d).contains("mesh shape"));
    }

    /// `.vfx.ron` v8 (ADR-7210): a v7 `Mesh` emitter keeps the built-in cube (no `mesh`), and
    /// re-saving it writes no `mesh` field.
    #[test]
    fn v7_mesh_particles_load_as_the_cube_at_v8() {
        let text = r#"
(version: 7, name: "shards", emitters: [(
    name: "e", spawn: (mode: Burst(4), lifetime: (min: 1.0, max: 1.0)),
    init: (size: (min: 0.2, max: 0.2)),
    render: (mode: Mesh),
)])"#;
        let def = VfxSystemDef::from_ron(text).expect("v7 loads");
        assert_eq!(def.version, 8);
        assert_eq!(def.emitters[0].render.mode, RenderMode::Mesh);
        assert_eq!(
            def.emitters[0].render.mesh, None,
            "a v7 mesh particle keeps the cube"
        );
        let saved = def.to_ron().expect("serialize");
        assert!(!saved.contains("mesh:"), "no mesh field for the cube: {saved}");
        assert!(saved.contains("version: 8"), "{saved}");
    }

    /// v8 `render.mesh`: round-trips, belongs to `Mesh` particles, and is never empty.
    #[test]
    fn v8_render_mesh_round_trips_and_is_validated() {
        let text = r#"
(version: 8, name: "rocks", emitters: [(
    name: "e", spawn: (mode: Burst(4), lifetime: (min: 1.0, max: 1.0)),
    init: (size: (min: 0.2, max: 0.2)),
    render: (mode: Mesh, mesh: Some("meshes/rock.mesh")),
)])"#;
        let def = VfxSystemDef::from_ron(text).expect("v8 loads");
        assert_eq!(def.emitters[0].render.mesh.as_deref(), Some("meshes/rock.mesh"));
        let back = VfxSystemDef::from_ron(&def.to_ron().expect("serialize")).expect("re-parse");
        assert_eq!(def, back);
        let reason = |def: VfxSystemDef| def.validate().expect_err("must be invalid").to_string();
        let mut d = def.clone();
        d.emitters[0].render.mesh = Some(String::new());
        assert!(reason(d).contains("must not be empty"));
        let mut d = def.clone();
        d.emitters[0].render.mode = RenderMode::Billboard;
        assert!(reason(d).contains("needs mode Mesh"));
        // A v7 reader refuses the v8 body (the version peek runs first).
        assert!(text.contains("version: 8"));
    }

    #[test]
    fn hex_colors_parse_linear() {
        let [r, g, b, a] = parse_color("#ff0000").expect("red");
        assert!((r - 1.0).abs() < 1e-5 && g < 1e-5 && b < 1e-5 && (a - 1.0).abs() < 1e-5);
        let [_, _, _, a] = parse_color("#ffffff80").expect("half alpha");
        assert!((a - 0.5).abs() < 0.01, "alpha {a}");
    }
}
