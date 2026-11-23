// SPDX-License-Identifier: MPL-2.0
//! Generators in the world (spec 62 § 7): the [`ProcGenerator`] component, the run lifecycle and
//! its events, world capture, applying outputs, and staleness.
//!
//! The lifecycle is one exclusive system, [`procgen_update`], so the agent protocol can drive the
//! same functions synchronously ([`request_run`], [`run_now`], [`cancel`], [`clear`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::{Quat, Vec3};
use regia_render::quality::QualityTier;
use regia_scene::components::{EntityGuid, Generated, Name, Tags};
use regia_scene::{GlobalTransform, Transform};

use crate::data::{GroundData, ShapeData, ShapeForm};
use crate::exec::{
    CancelToken, Evaluator, ProcCache, RunError, RunInputs, RunLimits, RunLimitsOverride, RunOutput,
    RunReport, RunResult, SelfInfo, SnapEntity, StaticSnapshot,
};
use crate::graph::{CompiledGraph, GraphError, resolve_graph};
use crate::nodes::ProcNodeRegistry;
use crate::seeds;
use crate::value::SettingValue;
use regia_scene::spawn::ReflectFormatVersion;

/// `format_version` of the saved [`ProcGenerator`] component (`procgen-generator`). Version 2
/// (spec 66, ADR-1875) adds `tiles`; a version 1 generator (v0.2.6) has no tiles and migrates by
/// setting the version ([`ProcGenerator::migrate`]).
pub const PROCGEN_GENERATOR_FORMAT_VERSION: u32 = 2;

/// The shape fingerprint of the saved `procgen-generator` component (`cargo xtask check-formats`):
/// field and variant names from reflection, so a field edit changes it.
pub fn procgen_generator_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            TypeInfo::Enum(e) => e.iter().map(|v| v.name()).collect::<Vec<_>>().join("|"),
            _ => String::new(),
        }
    }
    format!(
        "ProcGenerator({});ProcGenMode({});ProcBounds({});ProcBakeTier({});ProcLimits({});{}",
        names(ProcGenerator::type_info()),
        names(ProcGenMode::type_info()),
        names(ProcBounds::type_info()),
        names(ProcBakeTier::type_info()),
        names(ProcLimits::type_info()),
        crate::tiles::tile_settings_fingerprint()
    )
}

/// `procgen-generator` migration sources: version 1 (no `tiles`, [`ProcGenerator::migrate`]).
pub fn procgen_generator_migration_sources() -> Vec<u32> {
    vec![1]
}

/// When a generator runs (spec 62 § 7.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum ProcGenMode {
    /// Editor, agent or CLI only; the output is baked next to the level.
    #[default]
    Baked,
    /// At level load, on worker threads.
    OnLoad,
    /// At runtime in cells around anchors (spec 66).
    Anchored,
}

impl ProcGenMode {
    /// The agent name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Baked => "baked",
            Self::OnLoad => "on_load",
            Self::Anchored => "anchored",
        }
    }
}

/// The region a generator runs over.
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub enum ProcBounds {
    /// A box in entity space.
    Box {
        /// Half extents, meters.
        half_extents: Vec3,
    },
    /// The entity's own `Shape` component in world space (spec 64 § 2): boxes and spheres exactly,
    /// capsules and hulls as their bounding box. Without a `Shape` the run fails with
    /// `procgen_bounds_missing`.
    FromShape,
    /// The world-space box of the entity's own `Spline` grown by a margin on every side. Without a
    /// spline of two or more points the run fails with `procgen_bounds_missing`.
    FromSpline {
        /// Margin around the spline, meters.
        margin_m: f32,
    },
}

impl Default for ProcBounds {
    fn default() -> Self {
        Self::Box {
            half_extents: Vec3::new(32.0, 64.0, 32.0),
        }
    }
}

/// The quality tier a `Baked` run uses (spec 62 § 7.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum ProcBakeTier {
    /// Mobile Low.
    MobileLow,
    /// Mobile High.
    MobileHigh,
    /// Steam Deck.
    Deck,
    /// Desktop Low.
    Low,
    /// Desktop Medium.
    Medium,
    /// Desktop High (the default).
    #[default]
    High,
    /// Desktop Ultra.
    Ultra,
    /// One output per tier of [`ProcGenSettings::bake_tiers`] (`Regia.toml` `[procgen] bake_tiers`),
    /// each in its own `<guid>.<Tier>.pgout` ([`bake_tier_path`]); the loader picks the running tier's
    /// file, else the nearest lower tier, else the nearest higher one (spec 62 § 7.4). Editor
    /// previews and `OnLoad` runs use the running tier.
    PerTier,
}

impl ProcBakeTier {
    /// The renderer tier.
    pub fn tier(self) -> QualityTier {
        match self {
            Self::MobileLow => QualityTier::MobileLow,
            Self::MobileHigh => QualityTier::MobileHigh,
            Self::Deck => QualityTier::Deck,
            Self::Low => QualityTier::Low,
            Self::Medium => QualityTier::Medium,
            Self::High | Self::PerTier => QualityTier::High,
            Self::Ultra => QualityTier::Ultra,
        }
    }
}

/// Per-generator limit overrides (each capped by the platform limit).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub struct ProcLimits {
    /// Points per item.
    pub points_per_item: Option<u64>,
    /// Live elements across node outputs.
    pub total_elements: Option<u64>,
    /// Estimated bytes.
    pub memory_bytes: Option<u64>,
    /// Prefab instances.
    pub prefabs_per_generator: Option<u64>,
}

impl ProcLimits {
    fn to_override(self) -> RunLimitsOverride {
        let u = |v: Option<u64>| v.and_then(|x| usize::try_from(x).ok());
        RunLimitsOverride {
            points_per_item: u(self.points_per_item),
            total_elements: u(self.total_elements),
            memory_bytes: u(self.memory_bytes),
            prefabs_per_generator: u(self.prefabs_per_generator),
            ..RunLimitsOverride::default()
        }
    }
}

/// Runs a procedural graph over this entity's bounds (spec 62 § 7.1). Saved with the level
/// (`procgen-generator` v2); its output is not.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
pub struct ProcGenerator {
    /// Project-relative `.pgraph.ron` or `.pgvar.ron` path.
    #[reflect(@regia_ecs::attrs::AssetFilter("*.pgraph.ron, *.pgvar.ron"))]
    pub graph: String,
    /// Parameter overrides as RON text (`"2.0"`, `"\"meshes/a.glb\""`).
    pub params: BTreeMap<String, String>,
    /// Pinned seed; `None` = derived from the entity guid.
    pub seed: Option<u32>,
    /// When it runs.
    pub mode: ProcGenMode,
    /// The region it runs over.
    pub bounds: ProcBounds,
    /// Editor: re-run when stale (default true).
    pub live: bool,
    /// Runtime: re-run when stale (default false).
    pub refresh_on_change: bool,
    /// Limit overrides.
    pub limits: Option<ProcLimits>,
    /// Removed from cooked levels with its output.
    pub editor_only: bool,
    /// Grass-handoff order key, then guid. The run queue does not read it, and the value is not clamped.
    pub dependency_priority: i32,
    /// Tier for `Baked` runs (default High).
    pub quality_tier: Option<ProcBakeTier>,
    /// May apply terrain edits at bake time (spec 64 § 7.3).
    pub allow_terrain_edits: bool,
    /// Tiled generation (spec 66 § 2); `None` runs the whole bounds at once.
    pub tiles: Option<crate::tiles::TileSettings>,
    /// Always [`PROCGEN_GENERATOR_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for ProcGenerator {
    const CURRENT: u32 = PROCGEN_GENERATOR_FORMAT_VERSION;
}

impl Default for ProcGenerator {
    fn default() -> Self {
        Self {
            graph: String::new(),
            params: BTreeMap::new(),
            seed: None,
            mode: ProcGenMode::Baked,
            bounds: ProcBounds::default(),
            live: true,
            refresh_on_change: false,
            limits: None,
            editor_only: false,
            dependency_priority: 0,
            quality_tier: None,
            allow_terrain_edits: false,
            tiles: None,
            format_version: PROCGEN_GENERATOR_FORMAT_VERSION,
        }
    }
}

impl ProcGenerator {
    /// A generator for a graph path with defaults.
    pub fn new(graph: impl Into<String>) -> Self {
        Self {
            graph: graph.into(),
            ..Self::default()
        }
    }

    /// Upgrades an older saved shape. Version 1 had no `tiles` field: a v1 generator is untiled,
    /// which is what a missing `tiles` reads as, so the step only sets the version.
    ///
    /// # Errors
    ///
    /// A message for a version newer than this build reads.
    pub fn migrate(&mut self) -> Result<(), String> {
        if self.format_version > PROCGEN_GENERATOR_FORMAT_VERSION {
            return Err(format!(
                "procgen-generator format_version {} is newer than {PROCGEN_GENERATOR_FORMAT_VERSION}",
                self.format_version
            ));
        }
        self.format_version = PROCGEN_GENERATOR_FORMAT_VERSION;
        Ok(())
    }

    /// The parameter overrides parsed (`param_type_mismatch` for text that is not RON).
    ///
    /// # Errors
    ///
    /// The name of the first override whose text does not parse.
    pub fn parsed_params(&self) -> Result<BTreeMap<String, SettingValue>, String> {
        self.params
            .iter()
            .map(|(k, v)| {
                SettingValue::parse(v)
                    .map(|x| (k.clone(), x))
                    .map_err(|_| k.clone())
            })
            .collect()
    }
}

/// Run states (spec 62 § 7.2), also the agent strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcGenRunState {
    /// Never ran.
    #[default]
    Idle,
    /// Waiting for a run slot.
    Queued,
    /// Capturing world data.
    Capturing,
    /// Evaluating.
    Running,
    /// Swapping outputs.
    Applying,
    /// Output published.
    Ready,
    /// The last run failed (the previous output stays).
    Failed,
    /// The last run was cancelled (the previous output stays).
    Cancelled,
    /// Output removed.
    Cleared,
}

impl ProcGenRunState {
    /// The agent string.
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Queued => "queued",
            Self::Capturing => "capturing",
            Self::Running => "running",
            Self::Applying => "applying",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Cleared => "cleared",
        }
    }

    /// Whether a run is in flight.
    pub fn in_flight(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Capturing | Self::Running | Self::Applying
        )
    }
}

/// One captured dependency of a run (spec 62 § 5.3).
#[derive(Clone, Debug, PartialEq)]
pub enum ProcDependency {
    /// Terrain over an XZ rect `[min_x, min_z, max_x, max_z]`.
    Terrain {
        /// The captured rect.
        rect: [f32; 4],
    },
    /// An entity's transform or tags.
    Entity {
        /// Its guid.
        guid: Guid,
    },
    /// Entities matching an `in.entities` tag filter (entering or leaving the set marks stale).
    Query {
        /// All required.
        tags: Vec<String>,
        /// At least one required (when not empty).
        any_tags: Vec<String>,
        /// Guids matched at capture.
        matched: BTreeSet<Guid>,
    },
    /// A graph or variant file.
    File {
        /// Its path.
        path: String,
    },
    /// Another generator's published output (`in.generator_output`).
    Generator {
        /// Its guid.
        guid: Guid,
        /// Its output hash at capture.
        output_hash: Option<[u8; 32]>,
    },
}

/// A finished run's debug data (`procgen.inspect`).
pub type DebugOutputs = BTreeMap<u32, Vec<crate::Bundle>>;

/// A generator's cached `in.generator_output` names: valid for one file-check generation, graph path
/// and parameter text. A changed graph or parameter (a `Bind`-bound `generator`) resolves again at
/// once instead of at the next file check (corr-procgen).
#[derive(Debug, Clone, Default)]
pub(crate) struct GraphRefs {
    generation: u32,
    graph: String,
    params: BTreeMap<String, String>,
    refs: Vec<String>,
}

/// Runtime state of a generator (not saved).
#[derive(Component, Debug, Default)]
pub struct ProcGenState {
    /// Current state.
    pub state: ProcGenRunState,
    /// Id of the latest run (0 = none).
    pub run_id: u64,
    /// Id of the run whose output is published.
    pub last_ok_run: u64,
    /// Output hash of the published output.
    pub output_hash: Option<[u8; 32]>,
    /// Dependencies of the latest run.
    pub deps: Vec<ProcDependency>,
    /// Report of the latest finished run.
    pub report: Option<RunReport>,
    /// The cells of the published output (tiled generators), which a bake writes per cell.
    pub tiled: Option<Arc<crate::tiles::TiledRun>>,
    /// Cells a terrain edit touched since the published run (spec 66 § 2): the next run re-evaluates
    /// only these, unless `stale_whole` is set.
    pub stale_cells: BTreeSet<crate::tiles::TileKey>,
    /// A change the cell rectangles cannot localize (an entity, a file, another generator).
    pub stale_whole: bool,
    /// The queued run was started by staleness, not asked for: an edit's own consequence, not an
    /// edit (`terrain_edits::ProcTerrainEditLog`).
    pub refresh_run: bool,
    /// Debug outputs of the latest run made with `keep_debug`.
    pub debug: Option<Arc<DebugOutputs>>,
    /// The latest failure `(node, code, message)`.
    pub last_error: Option<(Option<u32>, String, String)>,
    /// A dependency changed since the latest run.
    pub stale: bool,
    /// Frames since it became stale (editor debounce).
    pub stale_frames: u32,
    /// Graph hash and parameter hash of the published output.
    pub hashes: Option<([u8; 32], [u8; 32])>,
    /// Number of graph evaluations (bakes loaded from disk do not count).
    pub evaluations: u64,
    /// The bake directory was checked for this generator's `.pgout`.
    pub bake_checked: bool,
    /// Content hashes of the files the latest run read (graph, variants, subgraphs, tables, road
    /// profiles, biome files), `None` for a file that could not be read: a changed hash marks the
    /// generator stale (spec 62 § 5.3).
    pub file_hashes: BTreeMap<String, Option<[u8; 32]>>,
    /// The `OnLoad` run of this generator was requested (once per spawn; spec 62 § 7.4).
    pub on_load_requested: bool,
    /// The tier of the published bake of a `PerTier` generator.
    pub baked_tier: Option<QualityTier>,
    /// The world bounds the latest run captured: a `FromShape`/`FromSpline` generator whose own shape or
    /// spline moves them becomes stale.
    pub run_bounds: Option<ShapeData>,
    /// Resolved `in.generator_output` names and what they were resolved from.
    pub(crate) graph_refs: Option<GraphRefs>,
    pub(crate) job: Option<Job>,
}

/// The published output of a generator (instances, prefab records, tables, points).
#[derive(Component, Debug, Clone)]
pub struct ProcOutput(pub Arc<RunOutput>);

impl ProcOutput {
    /// An exported table (`out.table`).
    pub fn table(&self, name: &str) -> Option<&crate::Table> {
        self.0.tables.get(name)
    }

    /// An exported mask (`out.mask`), e.g. the grass multiplier of spec 65 § 8.
    pub fn mask(&self, name: &str) -> Option<&crate::data::MaskData> {
        self.0.masks.get(name)
    }
}

/// The parent of a generator's spawned entities (identity transform; spec 62 § 7.3).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcOutputRoot {
    /// The generator entity.
    pub generator: Entity,
}

/// Marks an output entity that follows a data layer by name (spec 63 § 13.1 `target_layer`): the
/// static collider built for a layered instance set. [`follow_generator_layers`] parks it while
/// the layer is not `Live`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ProcLayerFollow {
    /// The layer name.
    pub layer: String,
}

/// Whether instances in `layer` are hidden now: the layer is known to the [`LayerTable`] and its
/// effective state is not `Live`. An unknown layer (or no table) hides nothing.
///
/// [`LayerTable`]: regia_scene::layers::LayerTable
pub fn layer_hidden(table: Option<&regia_scene::layers::LayerTable>, layer: &str) -> bool {
    table
        .and_then(|t| t.effective(layer))
        .is_some_and(|state| state != regia_scene::layers::LayerState::Live)
}

/// A generator's data layers and its presence (spec 37 § 4.1): the membership's layer names and
/// the lowest effective state among them (`Live` without a membership, a layer table or known
/// layers). `procgen.list` and `procgen.get` report both.
pub fn generator_presence(world: &World, entity: Entity) -> (Vec<String>, regia_scene::layers::LayerState) {
    use regia_scene::layers::{LayerMembership, LayerState, LayerTable};
    let Some(membership) = world.get::<LayerMembership>(entity) else {
        return (Vec::new(), LayerState::Live);
    };
    let presence = world
        .get_resource::<LayerTable>()
        .map_or(LayerState::Live, |t| t.presence(membership));
    (membership.layers.clone(), presence)
}

/// `PostUpdate`, after the layer switch: generator output follows its generator's data layers
/// (spec 62 § 7.3, ADR-7214). The generator is the only layer member; its output roots (whole
/// outputs and tile cells) are not. An output root whose generator is gone (an `Absent` member is
/// despawned) is despawned with it, and the respawned generator loads or runs its output again; a
/// root whose generator carries `LayerParked` is parked with its subtree (prefab instances and
/// instance colliders stop with it) and unparked when the generator is. Colliders of instance sets
/// with a `target_layer` ([`ProcLayerFollow`]) are parked while that layer is not `Live`.
pub fn follow_generator_layers(world: &mut World) {
    use regia_scene::layers::{LayerParked, LayerTable, set_parked};
    let roots: Vec<(Entity, Entity, bool)> = world
        .query::<(Entity, &ProcOutputRoot, Option<&LayerParked>)>()
        .iter(world)
        .map(|(e, r, p)| (e, r.generator, p.is_some_and(|m| !m.inherited)))
        .collect();
    let followers: Vec<(Entity, String, bool)> = world
        .query::<(Entity, &ProcLayerFollow, Option<&LayerParked>)>()
        .iter(world)
        .map(|(e, f, p)| (e, f.layer.clone(), p.is_some_and(|m| !m.inherited)))
        .collect();
    if roots.is_empty() && followers.is_empty() {
        return;
    }
    let _span = tracing::debug_span!("procgen.follow_layers", roots = roots.len()).entered();
    for (root, generator, parked) in roots {
        let Ok(gen_ref) = world.get_entity(generator) else {
            // The generator is gone (the layer switch despawned it, or it was deleted): its
            // output goes with it.
            if let Ok(e) = world.get_entity_mut(root) {
                e.despawn();
            }
            continue;
        };
        let want = gen_ref.contains::<LayerParked>();
        if want != parked {
            set_parked(world, root, want);
        }
    }
    for (entity, layer, parked) in followers {
        let want = layer_hidden(world.get_resource::<LayerTable>(), &layer);
        if want != parked {
            set_parked(world, entity, want);
        }
    }
}

/// `format_version` of the saved [`InstancedMeshSet`] component (`procgen-instanced-set`).
pub const INSTANCED_SET_FORMAT_VERSION: u32 = 1;

/// How a saved instance set collides (spec 63 § 13.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Reflect)]
pub enum InstanceCollision {
    /// No collision.
    #[default]
    None,
    /// A box per instance.
    Simple,
    /// The mesh's cooked collision mesh.
    Mesh,
}

/// One saved instance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Reflect)]
pub struct SavedInstance {
    /// Translation (relative to the set's entity).
    pub translation: Vec3,
    /// Rotation.
    pub rotation: Quat,
    /// Scale.
    pub scale: Vec3,
    /// Per-instance tint (linear RGBA).
    pub color: regia_core::math::Vec4,
    /// Up to four custom floats.
    pub custom: regia_core::math::Vec4,
}

/// Instances of one mesh saved as ordinary level content: what a detached instance set becomes
/// (spec 63 § 13.3, `procgen-instanced-set` v1). Drawn through the same instanced path as generator
/// output once that is hooked up.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, FormatVersion)]
pub struct InstancedMeshSet {
    /// Mesh asset.
    pub mesh: String,
    /// Material override.
    pub material: Option<String>,
    /// Casts shadows.
    pub cast_shadows: bool,
    /// Collision.
    pub collision: InstanceCollision,
    /// Draw distance, meters.
    pub draw_distance_m: f32,
    /// Fade band, meters.
    pub fade_m: f32,
    /// The instances.
    pub instances: Vec<SavedInstance>,
    /// Always [`INSTANCED_SET_FORMAT_VERSION`].
    pub format_version: u32,
}

impl regia_scene::spawn::FormatVersion for InstancedMeshSet {
    const CURRENT: u32 = INSTANCED_SET_FORMAT_VERSION;
}

impl Default for InstancedMeshSet {
    fn default() -> Self {
        Self {
            mesh: String::new(),
            material: None,
            cast_shadows: true,
            collision: InstanceCollision::None,
            draw_distance_m: 300.0,
            fade_m: 20.0,
            instances: Vec::new(),
            format_version: INSTANCED_SET_FORMAT_VERSION,
        }
    }
}

impl InstancedMeshSet {
    /// The saved form of a generator instance set.
    pub fn from_set(set: &crate::exec::InstanceSet) -> Self {
        Self {
            mesh: set.mesh.clone(),
            material: set.material.clone(),
            cast_shadows: set.cast_shadows,
            collision: match set.collision {
                crate::exec::Collision::None => InstanceCollision::None,
                crate::exec::Collision::Simple => InstanceCollision::Simple,
                crate::exec::Collision::Mesh => InstanceCollision::Mesh,
            },
            draw_distance_m: set.draw_distance_m,
            fade_m: set.fade_m,
            instances: set
                .instances
                .iter()
                .map(|i| SavedInstance {
                    translation: Vec3::from(i.translation),
                    rotation: Quat::from_array(i.rotation),
                    scale: Vec3::from(i.scale),
                    color: regia_core::math::Vec4::from(i.color),
                    custom: regia_core::math::Vec4::from(i.custom),
                })
                .collect(),
            format_version: INSTANCED_SET_FORMAT_VERSION,
        }
    }
}

/// The shape fingerprint of the saved `procgen-instanced-set` component (reflection names).
pub fn instanced_set_schema_fingerprint() -> String {
    use regia_ecs::reflect::{TypeInfo, Typed};
    fn names(info: &TypeInfo) -> String {
        match info {
            TypeInfo::Struct(s) => s.iter().map(|f| f.name()).collect::<Vec<_>>().join(","),
            TypeInfo::Enum(e) => e.iter().map(|v| v.name()).collect::<Vec<_>>().join("|"),
            _ => String::new(),
        }
    }
    format!(
        "InstancedMeshSet({});SavedInstance({});InstanceCollision({})",
        names(InstancedMeshSet::type_info()),
        names(SavedInstance::type_info()),
        names(InstanceCollision::type_info())
    )
}

/// `procgen-instanced-set` migration sources (none).
pub fn instanced_set_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// A prefab a generator spawned, by asset path (resolved by the prefab system when it is a
/// `guid:` reference).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ProcPrefabRef(pub String);

/// A run started.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ProcGenStarted {
    /// The generator.
    pub entity: Entity,
    /// The run.
    pub run_id: u64,
}

/// A run finished and its output is published.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ProcGenFinished {
    /// The generator.
    pub entity: Entity,
    /// The run.
    pub run_id: u64,
    /// Output hash.
    pub output_hash: [u8; 32],
    /// Instances, spawned entities, tables.
    pub counts: [usize; 3],
}

/// A run failed; the previous output stays.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ProcGenFailed {
    /// The generator.
    pub entity: Entity,
    /// The run.
    pub run_id: u64,
    /// The failing node (none for compile or capture failures).
    pub node: Option<u32>,
    /// Stable code.
    pub code: String,
}

/// A run was cancelled; the previous output stays.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ProcGenCancelled {
    /// The generator.
    pub entity: Entity,
    /// The run.
    pub run_id: u64,
}

/// A generator's output was removed.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ProcGenCleared {
    /// The generator.
    pub entity: Entity,
}

/// A generator's output became ordinary level content (`procgen.detach`).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ProcGenDetached {
    /// The generator.
    pub entity: Entity,
    /// The root of the detached content.
    pub new_root: Entity,
}

/// A terrain edit over an XZ rect: generators whose captured terrain overlaps it become stale.
/// Terrain editors (brush, agent, deformation) send it.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct ProcTerrainEdited {
    /// `[min_x, min_z, max_x, max_z]`.
    pub rect: [f32; 4],
    /// The generator whose own edit this is: that generator stays current (spec 64 § 7.3). `None`
    /// for an edit by the author or a tool.
    pub source: Option<Guid>,
}

/// The level-load gate finished: every `OnLoad` generator (and anchored generator with
/// `wait_on_load`) of the load is done, or the gate timed out (spec 62 § 7.4). A loading screen
/// waits for this after `LevelLoaded`; the report is also in [`ProcGenLoadGate::last`].
#[derive(Message, Debug, Clone, PartialEq)]
pub struct ProcGenLoadFinished {
    /// What the load ran.
    pub report: ProcGenLoadReport,
}

/// One generator of a level-load report.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcGenLoadEntry {
    /// The generator.
    pub entity: Entity,
    /// Its guid.
    pub guid: Guid,
    /// `on_load` or `anchored` (cells around the start anchor).
    pub mode: &'static str,
    /// Its state when the gate closed (`ready`, `failed`, ...).
    pub state: ProcGenRunState,
    /// Frames from the gate opening until it was done (the timeout when it timed out).
    pub frames: u32,
    /// Wall time from the gate opening until it was done, milliseconds (for reports, never for
    /// tests).
    pub ms: f64,
    /// The failure code (`procgen_on_load_timeout`, the run's code), if it failed.
    pub code: Option<String>,
}

/// What one level load generated (spec 62 § 7.4 "load report").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcGenLoadReport {
    /// The level whose `LevelLoaded` opened the gate, if one did.
    pub level: Option<regia_scene::level::LevelId>,
    /// The generators the load waited for, by guid.
    pub entries: Vec<ProcGenLoadEntry>,
    /// Frames the gate was open.
    pub frames: u32,
    /// Wall time the gate was open, milliseconds.
    pub ms: f64,
    /// The gate hit [`ProcGenSettings::on_load_timeout_frames`].
    pub timed_out: bool,
}

impl ProcGenLoadReport {
    /// Every entry ended `ready`.
    pub fn ok(&self) -> bool {
        !self.timed_out && self.entries.iter().all(|e| e.state == ProcGenRunState::Ready)
    }
}

/// The level-load gate (spec 62 § 7.4): open while `OnLoad` generators that spawned with a level (or
/// anchored generators with `wait_on_load`) have not finished their first run; closes with
/// [`ProcGenLoadFinished`].
#[derive(Resource, Debug, Default)]
pub struct ProcGenLoadGate {
    /// The open gate, if one is open.
    pub open: Option<ProcGenLoadReport>,
    /// The generators the open gate still waits for, with their kind (`on_load`, `anchored`).
    pub waiting: BTreeMap<Entity, &'static str>,
    /// The last closed gate's report.
    pub last: Option<ProcGenLoadReport>,
    /// The level of a `LevelLoaded` seen since the last gate opened.
    level: Option<regia_scene::level::LevelId>,
    started: Option<std::time::Instant>,
}

impl ProcGenLoadGate {
    /// A load is still generating: the level is not ready to show.
    pub fn pending(&self) -> bool {
        self.open.is_some()
    }
}

/// Frame counter of the file content check and the bake directory last seen (internal state of
/// [`procgen_update`]).
#[derive(Resource, Debug, Default)]
pub struct ProcGenWatch {
    /// Frames since the last file content check.
    pub frames_since_file_check: u32,
    /// The bake directory the generators' bakes were loaded from.
    pub bake_dir: Option<PathBuf>,
}

/// Caches rebuilt by [`detect_stale`] so an idle frame does not walk the level again.
#[derive(Resource, Default)]
struct ProcGenIntern {
    /// Bumped each time the file-content check is due. Generator-ref caches store this.
    file_check_generation: u32,
    /// Tagged entities from the last frame a finished generator had a query dependency.
    tag_snapshot: Vec<(Guid, Vec<String>)>,
    /// `tag_snapshot` matches the level until a tag change or a spawn/despawn.
    tags_valid: bool,
    /// Entity count when `tag_snapshot` was built (a despawn proxy).
    entity_len: u32,
}

fn intern_mut(world: &mut World) -> Mut<'_, ProcGenIntern> {
    if !world.contains_resource::<ProcGenIntern>() {
        world.insert_resource(ProcGenIntern::default());
    }
    world.resource_mut::<ProcGenIntern>()
}

/// Where graph files come from.
#[derive(Clone, Debug)]
pub enum ProcGraphSource {
    /// Files under a project directory.
    Dir(PathBuf),
    /// In-memory files (tests, the agent's unsaved edits).
    Memory(BTreeMap<String, String>),
}

impl ProcGraphSource {
    /// Reads a file's text.
    pub fn read(&self, path: &str) -> Option<String> {
        match self {
            Self::Dir(root) => {
                // Project-relative paths only: no absolute, drive, UNC or `..` paths, and no symlink
                // leading out of the project (ADR-1816).
                match regia_core::project_path::resolve_read(root, path) {
                    Ok(file) => std::fs::read_to_string(file).ok(),
                    Err(e) => {
                        if !matches!(e, regia_core::project_path::ProjectPathError::Io(_)) {
                            tracing::warn!(path, reason = e.code(), "graph path refused: {e}");
                        }
                        None
                    }
                }
            }
            Self::Memory(files) => files.get(path).cloned(),
        }
    }

    /// Reads a binary file under a project directory (`None` from memory, which holds text).
    pub fn read_bytes(&self, path: &str) -> Option<Vec<u8>> {
        match self {
            Self::Dir(root) => match regia_core::project_path::resolve_read(root, path) {
                Ok(file) => std::fs::read(file).ok(),
                Err(e) => {
                    if !matches!(e, regia_core::project_path::ProjectPathError::Io { .. }) {
                        tracing::warn!(path, reason = e.code(), "mask path refused: {e}");
                    }
                    None
                }
            },
            Self::Memory(_) => None,
        }
    }
}

/// CPU pixels of the mask textures a host holds, by asset path (`in.mask`, spec 63 § 3): the cook's CPU
/// copy or a loaded texture. A path it does not list is read from the project as a PNG.
#[derive(Resource, Default, Debug, Clone)]
pub struct ProcMaskLibrary(pub BTreeMap<String, crate::exec::MaskImage>);

/// Unsaved graph edits (`procgen.graph_edit` without `save`): read before the graph source, so
/// generators run the edited graph at once. Saving writes the file and drops the entry.
#[derive(Resource, Default, Debug, Clone)]
pub struct ProcGraphOverlay(pub BTreeMap<String, String>);

/// How many times [`read_graph_text`] has read a graph file in this process. A second anchored
/// schedule against an unchanged source does not increment it (the file check is throttled).
pub fn graph_text_reads() -> u64 {
    GRAPH_TEXT_READS.load(std::sync::atomic::Ordering::Relaxed)
}

static GRAPH_TEXT_READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The text of a graph or variant file: the overlay first, then the source.
pub fn read_graph_text(world: &World, path: &str) -> Option<String> {
    crate::probe::hit_graph_read();
    GRAPH_TEXT_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(t) = world
        .get_resource::<ProcGraphOverlay>()
        .and_then(|o| o.0.get(path))
    {
        return Some(t.clone());
    }
    world.get_resource::<ProcGenSettings>()?.source.read(path)
}

/// Generator settings (spec 62 § 7.2).
#[derive(Resource, Clone, Debug)]
pub struct ProcGenSettings {
    /// Runs at once (desktop 4, Android 1).
    pub max_concurrent_runs: usize,
    /// Evaluate on the main thread in the frame after capture (deterministic frame counts; tests and
    /// headless tools) instead of on the async compute pool.
    pub inline: bool,
    /// Editor rules for staleness (`live`) instead of runtime rules (`refresh_on_change`).
    pub editor: bool,
    /// Editor debounce before a stale live generator re-runs, in frames (150 ms at 60 Hz).
    pub debounce_frames: u32,
    /// Graph files.
    pub source: ProcGraphSource,
    /// Quality tier of `OnLoad`/`Anchored` runs and editor previews.
    pub running_tier: QualityTier,
    /// Node-output cache budget (0 disables).
    pub cache_bytes: usize,
    /// Keep every node's output for `procgen.inspect`.
    pub keep_debug: bool,
    /// Evaluation threads of generator runs: `None` = the compute pool, `Some(0)` = serial,
    /// `Some(n)` = a pool of `n` workers (spec 62 § 5.1).
    pub threads: Option<usize>,
    /// Where `Baked` generators keep their `.pgout` files (the level's `<level>.procgen/`
    /// directory, set by the level host); `None` = no bakes are loaded or written.
    pub bake_dir: Option<PathBuf>,
    /// The project's `[procgen] terrain_edits` key (`Regia.toml`, default false): whether any graph may
    /// change the heightfield at bake time (spec 64 § 7.3). Off for projects whose terrain heights a
    /// game simulation owns.
    pub terrain_edits: bool,
    /// Freezes anchored scheduling (resident cells stay) for cinematics and render-out (spec 66 § 5).
    pub paused: bool,
    /// The tiers a `PerTier` generator bakes (`Regia.toml` `[procgen] bake_tiers`, default all seven;
    /// spec 62 § 7.4). [`parse_bake_tiers`] reads the key's names.
    pub bake_tiers: Vec<QualityTier>,
    /// Frames the level-load gate waits for `OnLoad` generators (and anchored generators with
    /// `wait_on_load`) before it fails them with `procgen_on_load_timeout` (default 600, ten seconds at
    /// 60 Hz; counted in frames so tests are deterministic).
    pub on_load_timeout_frames: u32,
    /// Frames between two content-hash checks of the files generators read (default 30; 0 checks every
    /// frame). Only stale generators with `live` (editor) or `refresh_on_change` (runtime) re-run.
    pub file_check_frames: u32,
}

/// Editor cache budget (spec 62 § 5): 512 MiB.
pub const EDITOR_CACHE_BYTES: usize = 512 << 20;

/// The cache budget the plugin builds: an editor session whose `cache_bytes` is still the runtime
/// default gets the editor budget; an explicit other value is kept.
pub fn effective_cache_bytes(settings: &ProcGenSettings) -> usize {
    if settings.editor && settings.cache_bytes == ProcGenSettings::default().cache_bytes {
        EDITOR_CACHE_BYTES
    } else {
        settings.cache_bytes
    }
}

impl Default for ProcGenSettings {
    fn default() -> Self {
        let android = cfg!(target_os = "android");
        Self {
            max_concurrent_runs: if android { 1 } else { 4 },
            inline: false,
            editor: false,
            debounce_frames: 9,
            source: ProcGraphSource::Dir(PathBuf::from(".")),
            running_tier: QualityTier::High,
            cache_bytes: if android { 16 << 20 } else { 64 << 20 },
            keep_debug: false,
            threads: None,
            bake_dir: None,
            terrain_edits: false,
            paused: false,
            bake_tiers: QualityTier::ALL.to_vec(),
            on_load_timeout_frames: 600,
            file_check_frames: 30,
        }
    }
}

/// Parses `[procgen] bake_tiers` names (`"MobileLow"`, `"high"`, `"mobile_high"`, ...) into tiers,
/// cheapest first without repeats; an empty list means all seven tiers.
///
/// # Errors
///
/// `procgen_unknown_tier` with the first name that is not a tier.
pub fn parse_bake_tiers<S: AsRef<str>>(names: &[S]) -> Result<Vec<QualityTier>, BakeError> {
    let mut tiers = BTreeSet::new();
    for n in names {
        let t = QualityTier::from_name(n.as_ref()).ok_or_else(|| {
            (
                "procgen_unknown_tier".to_string(),
                format!("`{}` is not a quality tier", n.as_ref()),
            )
        })?;
        tiers.insert(crate::nodes::quality_index(t));
    }
    if tiers.is_empty() {
        return Ok(QualityTier::ALL.to_vec());
    }
    Ok(tiers.into_iter().map(|i| QualityTier::ALL[i]).collect())
}

/// A terrain map for headless runs (tools, tests) when the world has no `Terrain` component.
#[derive(Resource, Clone, Debug)]
pub struct ProcGenTerrain(pub regia_terrain::TerrainMap);

/// The node registry used by generators (`procgen.node_types`).
#[derive(Resource, Clone)]
pub struct ProcGenRegistry(pub Arc<ProcNodeRegistry>);

impl Default for ProcGenRegistry {
    fn default() -> Self {
        Self(Arc::new(ProcNodeRegistry::with_builtins()))
    }
}

/// The shared node-output cache.
#[derive(Resource, Clone)]
pub struct ProcGenCache(pub Arc<Mutex<ProcCache>>);

/// The ordered lifecycle event log (`procgen.started` ... `procgen.cleared`), newest last, capped at
/// [`ProcGenLog::CAP`]: what the agent event stream forwards, and what tests read for ordering.
#[derive(Resource, Default, Debug)]
pub struct ProcGenLog(pub Vec<(Entity, &'static str, u64)>);

impl ProcGenLog {
    /// Entries kept.
    pub const CAP: usize = 1024;
}

fn log(world: &mut World, entity: Entity, kind: &'static str, run_id: u64) {
    let mut l = world.resource_mut::<ProcGenLog>();
    if l.0.len() >= ProcGenLog::CAP {
        l.0.remove(0);
    }
    l.0.push((entity, kind, run_id));
}

/// Explicit run requests waiting for a slot. The lifecycle starts them in `EntityGuid`
/// order each frame (guid-less last), not in request order: the first applier of an
/// overlapping terrain edit wins, and guid order is the only start order a platform with
/// fewer run slots (Android defaults to one) cannot diverge on — there, a lower-guid run
/// queued behind a higher-guid one would otherwise apply second and own the samples
/// (ADR-5447).
#[derive(Resource, Default, Debug)]
pub struct ProcGenQueue(pub Vec<Entity>);

/// An in-flight run.
pub(crate) struct Job {
    run_id: u64,
    cancel: CancelToken,
    pending: JobState,
    graph_hash: [u8; 32],
    param_hash: [u8; 32],
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

type JobResult = Result<(RunResult, Option<crate::tiles::TiledRun>), RunError>;

/// What a tiled run needs besides the graph and inputs (spec 66 § 2).
#[derive(Clone)]
pub(crate) struct TileCtx {
    pub(crate) plan: crate::tiles::TierPlan,
    pub(crate) bounds: ShapeData,
    pub(crate) halo_m: f64,
    /// Re-run only these cells over this finished run.
    pub(crate) partial: Option<(Arc<crate::tiles::TiledRun>, Vec<crate::tiles::TileKey>)>,
}

/// Evaluates a whole run: untiled, or every cell of every tier (spec 66 § 2).
fn evaluate(
    eval: &Evaluator,
    compiled: &CompiledGraph,
    inputs: &RunInputs,
    tile: Option<&TileCtx>,
    cancel: &CancelToken,
) -> JobResult {
    let Some(t) = tile else {
        return eval.run(compiled, inputs, cancel).map(|r| (r, None));
    };
    let job = crate::tiles::TileJob {
        evaluator: eval,
        graph: compiled,
        plan: &t.plan,
        inputs,
        bounds: &t.bounds,
        halo_m: t.halo_m,
    };
    let tiled = match &t.partial {
        Some((previous, keys)) => crate::tiles::rerun_cells(&job, previous, keys, cancel)?,
        None => crate::tiles::run_tiled(&job, 1, cancel)?,
    };
    let mut report = RunReport {
        compile_warnings: compiled.warnings.clone(),
        ..RunReport::default()
    };
    for c in tiled.cells.values() {
        report.absorb(&c.report);
    }
    let result = RunResult {
        output: tiled.output.clone(),
        output_hash: tiled.output_hash,
        report,
        debug: None,
    };
    Ok((result, Some(tiled)))
}

/// The tile context of a generator with [`ProcGenerator::tiles`], or `(code, message)`.
pub(crate) fn tile_ctx(
    generator: &ProcGenerator,
    compiled: &CompiledGraph,
    bounds: ShapeData,
) -> Result<Option<TileCtx>, (String, String)> {
    let Some(ts) = &generator.tiles else {
        return Ok(None);
    };
    ts.validate()?;
    let plan = crate::tiles::TierPlan::build(compiled, f64::from(ts.cell_m)).map_err(|e| {
        let code =
            e.0.first()
                .map_or("procgen_graph_invalid".to_string(), |g| g.code.clone());
        (code, e.to_string())
    })?;
    let halo_m = ts.halo(plan.reach_m);
    Ok(Some(TileCtx {
        plan,
        bounds,
        halo_m,
        partial: None,
    }))
}

/// Whether a graph reads the ground farther from an element than the halo reaches: a road row
/// takes its height from the whole road, and a flattened sample its target from the road surface
/// near it. A terrain edit under such a graph cannot be localized by the halo. A subgraph may hold
/// the same nodes.
fn reads_past_halo(compiled: &CompiledGraph) -> bool {
    compiled.nodes.iter().any(|n| {
        let id = n.kind.id();
        id.starts_with("road.")
            || id == "terrain.flatten_road"
            || id == "graph.call"
            // A pad's mean height reads the whole pad; a warp reads `strength_m` away (ADR-8465).
            || id == "terrain.flatten_pad"
            || id == "terrain.domain_warp"
    })
}

/// What a tiled generator's next run may reuse: the published cells, when only terrain edits
/// happened since and nothing else about the run changed.
fn partial_run(
    world: &World,
    entity: Entity,
    tile: &TileCtx,
    compiled: &CompiledGraph,
    hashes: ([u8; 32], [u8; 32]),
    key: (u32, QualityTier),
) -> Option<(Arc<crate::tiles::TiledRun>, Vec<crate::tiles::TileKey>)> {
    let st = world.get::<ProcGenState>(entity)?;
    let run = st.tiled.as_ref()?;
    let same = !reads_past_halo(compiled)
        && st.hashes == Some(hashes)
        && run.key == key
        && run.bounds == tile.bounds
        && run.halo_m == tile.halo_m
        && !st.stale_whole
        && !st.stale_cells.is_empty();
    same.then(|| (run.clone(), st.stale_cells.iter().copied().collect()))
}

enum JobState {
    /// Inline: evaluated in the next update.
    Inline(Box<(CompiledGraph, RunInputs, Option<TileCtx>)>),
    /// Threaded: the result arrives on a channel.
    Thread(Mutex<std::sync::mpsc::Receiver<JobResult>>),
}

/// System set of the generator lifecycle (runs in `Update`).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProcGenSet {
    /// Anchored runtime generation (spec 66 § 4.3): plan, start, poll and apply cells.
    Schedule,
    /// Requests, capture, evaluation polling and apply.
    Lifecycle,
}

/// Registers the generator types, messages, resources and the lifecycle system.
#[derive(Default)]
pub struct ProcGenPlugin;

impl Plugin for ProcGenPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<ProcGenerator>()
            .register_type::<ProcGenMode>()
            .register_type::<ProcBounds>()
            .register_type::<ProcBakeTier>()
            .register_type::<ProcLimits>()
            .register_type::<InstancedMeshSet>()
            .register_type::<SavedInstance>()
            .register_type::<InstanceCollision>()
            .register_type::<crate::painted::PaintedPoints>()
            .register_type::<crate::painted::PaintedPoint>()
            .register_type::<crate::road::RoadOf>()
            .register_type::<crate::biome::ecs::BiomeArea>()
            .register_type::<crate::biome::ecs::AreaShape>()
            .register_type::<crate::tiles::TileSettings>()
            .register_type::<crate::tiles::AnchorSettings>()
            .register_type::<crate::tiles::AnchorSource>()
            .register_type::<crate::tiles::TierReach>()
            .register_type::<crate::tiles::ViewGate>()
            .register_type::<crate::tiles::ProcGenAnchor>()
            .add_message::<ProcGenStarted>()
            .add_message::<ProcGenFinished>()
            .add_message::<ProcGenFailed>()
            .add_message::<ProcGenCancelled>()
            .add_message::<ProcGenCleared>()
            .add_message::<ProcGenDetached>()
            .add_message::<ProcTerrainEdited>()
            .add_message::<ProcGenLoadFinished>()
            .add_message::<regia_scene::level::LevelLoaded>();
        app.init_resource::<ProcGenRegistry>()
            .init_resource::<ProcGenQueue>()
            .init_resource::<ProcGenLoadGate>()
            .init_resource::<ProcGenWatch>()
            .init_resource::<ProcLevelBakeDirs>()
            .init_resource::<ProcGenIntern>()
            .init_resource::<ProcGenLog>()
            .init_resource::<ProcGraphOverlay>();
        if !app.world().contains_resource::<ProcGenSettings>() {
            app.init_resource::<ProcGenSettings>();
        }
        let bytes = effective_cache_bytes(app.world().resource::<ProcGenSettings>());
        app.insert_resource(ProcGenCache(Arc::new(Mutex::new(ProcCache::new(bytes)))));
        app.init_resource::<crate::tiles::ProcGenTiles>();
        app.configure_sets(Update, ProcGenSet::Schedule.before(ProcGenSet::Lifecycle));
        app.add_systems(
            Update,
            crate::tiles::procgen_schedule.in_set(ProcGenSet::Schedule),
        );
        app.add_systems(
            Update,
            (
                despawn_orphan_outputs,
                procgen_level_loaded,
                procgen_update,
                refresh_instance_colliders,
                sync_output_root_layers,
            )
                .chain()
                .in_set(ProcGenSet::Lifecycle),
        );
        app.add_plugins(crate::prefab_overrides::ProcPrefabOverridesPlugin);
        // Biome grass multipliers into the terrain's grass bake (spec 65 § 8).
        app.add_systems(
            Update,
            (
                crate::biome::ecs::biome_grass_handoff,
                // The host's grass re-bakes on a multiplier change (ADR-7214).
                crate::biome::ecs::grass_rebake,
            )
                .chain()
                .in_set(ProcGenSet::Lifecycle)
                .after(procgen_update),
        );
        // Output follows the generator's data layers (spec 62 § 7.3, ADR-7214).
        app.add_systems(
            PostUpdate,
            follow_generator_layers
                .after(regia_scene::layers::LayerSwitch)
                .before(regia_app::PostUpdateSet::TransformPropagate),
        );
        // The tile and region overlays (spec 66 § 7), drawn only while the gizmo group is on.
        app.init_resource::<crate::debug_view::ProcGenDebug>()
            .init_resource::<crate::debug_view::ProcGenOverlay>()
            .add_systems(
                PostUpdate,
                (
                    crate::debug_view::collect_overlay,
                    crate::debug_view::draw_overlay,
                )
                    .chain()
                    .run_if(regia_render::gizmos::gizmo_group_enabled::<crate::debug_view::ProcGenGizmos>),
            );
        // Instance sets on the frame (between the frame's mesh LOD selection and glass extraction,
        // like the runtime scatter, ADR-1415).
        app.init_resource::<crate::draw::ProcDrawStats>().add_systems(
            Render,
            crate::draw::extract_proc_instances
                .after(regia_render::select_mesh_lods)
                .before(regia_render::extract_glass),
        );
    }

    fn finish(&self, app: &mut App) {
        // The gizmo group hands its lines to the renderer; register it only when the renderer's gizmo
        // plugin is present, so headless apps (servers, tests) run without it.
        if app.has_plugin(std::any::type_name::<regia_render::gizmos::GizmoPlugin>()) {
            use regia_render::gizmos::AppGizmoBuilder;
            app.init_gizmo_group::<crate::debug_view::ProcGenGizmos>();
        }
    }
}

/// The terrain-edit gate of a generator's run (spec 64 § 7.3): `Baked`, `allow_terrain_edits` and
/// the project's `[procgen] terrain_edits` ([`ProcGenSettings::terrain_edits`]).
pub fn terrain_edit_gate(
    generator: &ProcGenerator,
    settings: &ProcGenSettings,
) -> crate::exec::TerrainEditGate {
    use crate::exec::TerrainEditGate;
    // Tiled generators with anchors stream their cells: no bake step owns the terrain (spec 64 § 7.3).
    let anchored = generator.tiles.as_ref().is_some_and(|t| t.runtime.is_some());
    if generator.mode != ProcGenMode::Baked || anchored {
        TerrainEditGate::NotBaked
    } else if generator.allow_terrain_edits && settings.terrain_edits {
        TerrainEditGate::Allowed
    } else {
        TerrainEditGate::NotAllowed
    }
}

/// Whether a generator is `editor_only` in a host that is not the editor: it never runs, loads a
/// bake or publishes output there (`procgen_editor_only`), and the cook leaves it out of packaged
/// levels ([`crate::cook`]). Bakes and verifies (tools) still run it.
pub fn editor_only_refused(world: &World, entity: Entity) -> bool {
    world.get::<ProcGenerator>(entity).is_some_and(|g| g.editor_only)
        && !world.get_resource::<ProcGenSettings>().is_some_and(|s| s.editor)
}

/// The generator seed: the pinned seed or the guid-derived one (spec 62 § 6.2). A `Baked`
/// inline-placed member whose bake lives in its source level's directory derives it from its
/// source guid (ADR-7331, [`bake_target`]).
pub fn generator_seed(world: &World, entity: Entity) -> u32 {
    let pinned = world.get::<ProcGenerator>(entity).and_then(|g| g.seed);
    pinned.unwrap_or_else(|| seed_guid(world, entity).map_or(0, seeds::gen_seed_from_guid))
}

pub(crate) fn state_mut(world: &mut World, entity: Entity) -> Option<Mut<'_, ProcGenState>> {
    if world.get::<ProcGenState>(entity).is_none() {
        world.get_entity_mut(entity).ok()?.insert(ProcGenState::default());
    }
    world.get_mut::<ProcGenState>(entity)
}

/// Cancels an in-flight run of `entity` and sends `Cancelled`. Returns the cancelled run id.
pub fn cancel(world: &mut World, entity: Entity) -> Option<u64> {
    let mut st = state_mut(world, entity)?;
    if !st.state.in_flight() {
        return None;
    }
    let run_id = st.run_id;
    if let Some(job) = st.job.take() {
        job.cancel.cancel();
    }
    st.state = ProcGenRunState::Cancelled;
    world.resource_mut::<ProcGenQueue>().0.retain(|e| *e != entity);
    world.write_message(ProcGenCancelled { entity, run_id });
    log(world, entity, "cancelled", run_id);
    Some(run_id)
}

/// A scheduled generator has no whole-graph job (spec 66 § 4.2a). A `Queued` state with no worker
/// is a leftover run request: put it back to `Ready` when output is published, otherwise `Idle`,
/// and drop it from the whole-graph queue so dependents are not blocked.
fn release_scheduled_queue(world: &mut World, entity: Entity) {
    let has_output = world.get::<ProcOutput>(entity).is_some();
    if let Some(mut st) = world.get_mut::<ProcGenState>(entity)
        && st.state == ProcGenRunState::Queued
        && st.job.is_none()
    {
        st.state = if has_output {
            ProcGenRunState::Ready
        } else {
            ProcGenRunState::Idle
        };
    }
    world.resource_mut::<ProcGenQueue>().0.retain(|e| *e != entity);
}

/// Drops a leftover whole-graph queue entry and re-queues resident, applying and failed cells.
/// The caller does not start a whole-graph run (spec 66 § 4.2a).
pub fn requeue_scheduled(world: &mut World, entity: Entity) -> Vec<crate::tiles::TileKey> {
    release_scheduled_queue(world, entity);
    crate::tiles::rebuild_cells(world, entity, None, None)
}

/// Queues a run (cancelling one in flight first: `Cancelled(old)` precedes `Started(new)`).
/// A scheduled generator re-queues its cells instead (spec 66 § 4.2a) and is not enqueued here.
pub fn request_run(world: &mut World, entity: Entity) -> bool {
    if world.get::<ProcGenerator>(entity).is_none() {
        return false;
    }
    if world
        .get::<ProcGenerator>(entity)
        .is_some_and(crate::tiles::is_scheduled)
    {
        requeue_scheduled(world, entity);
        return true;
    }
    cancel(world, entity);
    if let Some(mut st) = state_mut(world, entity) {
        st.state = ProcGenRunState::Queued;
        st.refresh_run = false;
    }
    let mut q = world.resource_mut::<ProcGenQueue>();
    if !q.0.contains(&entity) {
        q.0.push(entity);
    }
    true
}

/// Removes a generator's output (cancelling a run first) and sends `Cleared`.
pub fn clear(world: &mut World, entity: Entity) -> bool {
    if world.get::<ProcGenerator>(entity).is_none() {
        return false;
    }
    cancel(world, entity);
    despawn_outputs(world, entity);
    crate::terrain_edits::logged(world, entity, |w| crate::terrain_edits::restore(w, entity));
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.remove::<(ProcOutput, ProcBakeOrigin)>();
    }
    if let Some(mut st) = state_mut(world, entity) {
        st.state = ProcGenRunState::Cleared;
        st.output_hash = None;
        st.hashes = None;
        st.last_ok_run = 0;
        st.tiled = None;
    }
    world.write_message(ProcGenCleared { entity });
    log(world, entity, "cleared", 0);
    true
}

/// Despawns the output roots (whole outputs and tile cells, with everything under them) of
/// generators that are gone: despawned, for example by an `Absent` layer switch of a generator with
/// a level template record (the switch back respawns the generator, which applies a new output), or
/// stripped of `ProcGenerator`. Idle frames only read the removal list.
fn despawn_orphan_outputs(
    mut removed: RemovedComponents<ProcGenerator>,
    roots: Query<(Entity, &ProcOutputRoot)>,
    generators: Query<(), With<ProcGenerator>>,
    mut commands: Commands,
) {
    if removed.read().count() == 0 {
        return;
    }
    for (root, r) in &roots {
        if !generators.contains(r.generator) {
            commands.entity(root).despawn();
        }
    }
}

fn despawn_outputs(world: &mut World, entity: Entity) {
    let roots: Vec<Entity> = world
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(world)
        .filter(|(_, r)| r.generator == entity)
        .map(|(e, _)| e)
        .collect();
    for root in roots {
        if let Ok(e) = world.get_entity_mut(root) {
            e.despawn();
        }
    }
}

/// An entity's world transform: its `Transform` when it has no parent (propagation may not have run
/// yet), else its `GlobalTransform`.
pub(crate) fn world_transform(
    gt: Option<&GlobalTransform>,
    t: Option<&Transform>,
    has_parent: bool,
) -> Transform {
    match (has_parent, gt, t) {
        (true, Some(g), _) => g.to_transform(),
        (_, _, Some(t)) => *t,
        (_, Some(g), None) => g.to_transform(),
        _ => Transform::default(),
    }
}

/// A spline moved to world space with scalar math only (deterministic on every target): positions
/// through scale, rotation and translation, tangents through scale and rotation.
pub fn world_spline(s: &regia_scene::spline::Spline, tf: &Transform) -> regia_scene::spline::Spline {
    let m = crate::rot::Rot(tf.rotation.to_array()).to_mat3();
    let lin = |v: Vec3| m * (v * tf.scale);
    let mut out = s.clone();
    for p in &mut out.points {
        p.position = lin(p.position) + tf.translation;
        p.tangent_in = lin(p.tangent_in);
        p.tangent_out = lin(p.tangent_out);
    }
    out
}

/// A `Shape` as a world-space region item: boxes and spheres exactly (a sphere takes the largest
/// axis scale), capsules and hulls as their local bounding box.
pub fn world_shape(s: &regia_scene::shape::Shape, tf: &Transform) -> ShapeData {
    use regia_scene::shape::ShapeKind;
    let sc = tf.scale.abs();
    let (center, form) = match &s.kind {
        ShapeKind::Box { half_extents } => (
            Vec3::ZERO,
            ShapeForm::Box {
                // A negative half extent is 0, the same rule as `PreparedShape`.
                half_extents: (*half_extents * sc).max(Vec3::ZERO).to_array(),
            },
        ),
        ShapeKind::Sphere { radius } => (
            Vec3::ZERO,
            ShapeForm::Sphere {
                radius: (radius * sc.max_element()).max(0.0),
            },
        ),
        ShapeKind::Capsule { radius, half_height } => (
            Vec3::ZERO,
            ShapeForm::Box {
                half_extents: (Vec3::new(*radius, half_height + radius, *radius) * sc)
                    .max(Vec3::ZERO)
                    .to_array(),
            },
        ),
        ShapeKind::Hull { points } => {
            let (lo, hi) = points.iter().fold(
                (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
                |(lo, hi), p| (lo.min(*p), hi.max(*p)),
            );
            if points.is_empty() {
                (
                    Vec3::ZERO,
                    ShapeForm::Box {
                        half_extents: [0.0; 3],
                    },
                )
            } else {
                (
                    (lo + hi) * 0.5,
                    ShapeForm::Box {
                        half_extents: ((hi - lo) * 0.5 * sc).to_array(),
                    },
                )
            }
        }
    };
    let m = crate::rot::Rot(tf.rotation.to_array()).to_mat3();
    ShapeData {
        form,
        position: (m * (center * tf.scale) + tf.translation).to_array(),
        rotation: tf.rotation.to_array(),
        edge_m: s.edge_m.max(0.0),
        clip_xz: None,
    }
}

/// Whether `tags` passes an `in.entities` filter.
fn tags_match(tags: &[String], all: &[String], any: &[String]) -> bool {
    all.iter().all(|t| tags.contains(t)) && (any.is_empty() || any.iter().any(|t| tags.contains(t)))
}

/// Why a generator's bounds cannot be computed: `FromShape` without a `Shape`, `FromSpline` without
/// a spline of two or more points (`procgen_bounds_missing`, the message).
pub fn bounds_problem(world: &World, entity: Entity) -> Option<String> {
    match world.get::<ProcGenerator>(entity).map(|g| g.bounds)? {
        ProcBounds::FromShape if world.get::<regia_scene::shape::Shape>(entity).is_none() => {
            Some("bounds are FromShape but the generator has no Shape component".to_string())
        }
        ProcBounds::FromSpline { .. }
            if world
                .get::<regia_scene::spline::Spline>(entity)
                .is_none_or(|s| s.points.len() < 2) =>
        {
            Some("bounds are FromSpline but the generator has no Spline with two or more points".to_string())
        }
        _ => None,
    }
}

/// The world-space bounds of a generator (spec 62 § 7.1): its box; its own `Shape` (`FromShape`);
/// or the world box of its own `Spline` plus the margin (`FromSpline`). A missing shape or spline
/// gives a 0.5 m box ([`bounds_problem`] names it, and a run fails).
pub fn generator_bounds(world: &World, entity: Entity) -> ShapeData {
    let tf = world_transform(
        world.get::<GlobalTransform>(entity),
        world.get::<Transform>(entity),
        world.get::<ChildOf>(entity).is_some(),
    );
    let (pos, rot, scale) = (tf.translation, tf.rotation, tf.scale);
    let bounds = world.get::<ProcGenerator>(entity).map(|g| g.bounds);
    match bounds {
        Some(ProcBounds::FromShape) => {
            if let Some(shape) = world.get::<regia_scene::shape::Shape>(entity) {
                return world_shape(shape, &tf);
            }
        }
        Some(ProcBounds::FromSpline { margin_m }) => {
            if let Some(spline) = world.get::<regia_scene::spline::Spline>(entity)
                && spline.points.len() >= 2
            {
                let (lo, hi) = world_spline(spline, &tf).bounds();
                let m = Vec3::splat(if margin_m.is_finite() {
                    margin_m.max(0.0)
                } else {
                    0.0
                });
                let (lo, hi) = (lo - m, hi + m);
                return ShapeData {
                    form: ShapeForm::Box {
                        half_extents: ((hi - lo) * 0.5).to_array(),
                    },
                    position: ((lo + hi) * 0.5).to_array(),
                    rotation: [0.0, 0.0, 0.0, 1.0],
                    edge_m: 0.0,
                    clip_xz: None,
                };
            }
        }
        _ => {}
    }
    let half = match bounds {
        Some(ProcBounds::Box { half_extents }) => half_extents,
        Some(ProcBounds::FromSpline { margin_m }) => Vec3::splat(0.5 + margin_m.max(0.0)),
        _ => Vec3::splat(0.5),
    };
    ShapeData {
        form: ShapeForm::Box {
            half_extents: (half * scale.abs()).max(Vec3::ZERO).to_array(),
        },
        position: pos.to_array(),
        rotation: rot.to_array(),
        edge_m: 0.0,
        clip_xz: None,
    }
}

/// Captures what a run reads (spec 62 § 5.3) and its dependencies: terrain only when a node samples
/// the ground, entities only when an `in.entities` filter matches them.
pub(crate) fn capture(
    world: &mut World,
    entity: Entity,
    compiled: &CompiledGraph,
) -> (StaticSnapshot, Vec<ProcDependency>) {
    // The generator's own terrain edit is not an input (spec 64 § 7.3).
    crate::terrain_edits::lifted(world, entity, |world| capture_inner(world, entity, compiled))
}

fn capture_inner(
    world: &mut World,
    entity: Entity,
    compiled: &CompiledGraph,
) -> (StaticSnapshot, Vec<ProcDependency>) {
    let bounds = generator_bounds(world, entity);
    let (lo, hi) = bounds.world_aabb();
    let rect = [lo[0], lo[2], hi[0], hi[2]];
    let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    let name = world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default();
    let uses = |ids: &[&str]| compiled.nodes.iter().any(|n| ids.contains(&n.kind.id()));
    let mut deps = Vec::new();
    let ground = if uses(&[
        "in.ground",
        "sample.ground",
        "sample.poisson",
        "points.flat_only",
        "biome.layer",
    ]) {
        // With several terrains, the lowest guid wins (guid-less ones last, then entity order):
        // query order follows spawn and archetype history, which a save/load cycle changes (audit
        // L13).
        let mut terrains: Vec<(bool, Guid, Entity)> = world
            .query_filtered::<(Entity, Option<&EntityGuid>), With<regia_terrain::Terrain>>()
            .iter(world)
            .map(|(e, g)| (g.is_none(), g.map_or(Guid::NIL, |g| g.0), e))
            .collect();
        terrains.sort();
        let primary = terrains.first().map(|(_, _, e)| *e);
        let from_component = primary
            .and_then(|e| world.get::<regia_terrain::Terrain>(e))
            .map(|t| Arc::new(GroundData::capture(&t.map, rect, 2)));
        let g = from_component.or_else(|| {
            world
                .get_resource::<ProcGenTerrain>()
                .map(|t| Arc::new(GroundData::capture(&t.0, rect, 2)))
        });
        if g.is_some() {
            deps.push(ProcDependency::Terrain { rect });
        }
        g.map(|g| capture_ground_layers(world, compiled, g, primary))
    } else {
        None
    };
    let filters: Vec<(Vec<String>, Vec<String>)> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.entities")
        .map(|n| (n.settings.names("tags"), n.settings.names("any_tags")))
        .collect();
    let mut entities = Vec::new();
    if !filters.is_empty() {
        let mut q = world.query_filtered::<(
            Entity,
            &EntityGuid,
            Option<&GlobalTransform>,
            Option<&Transform>,
            Option<&Tags>,
            Has<ChildOf>,
            (
                Option<&regia_scene::spline::Spline>,
                Option<&regia_scene::shape::Shape>,
                Option<&crate::road::RoadOf>,
                Option<&regia_render::MeshRenderer>,
            ),
        ), Without<Generated>>();
        // Mesh bounds once per shared mesh (`Arc` identity).
        let mut mesh_bounds: BTreeMap<usize, Option<LocalBounds>> = BTreeMap::new();
        for (e, g, gt, t, tags, parent, (spline, shape, road_of, renderer)) in q.iter(world) {
            if e == entity {
                continue;
            }
            let tags: Vec<String> = tags
                .map(|t| t.0.iter().map(|n| n.as_str().to_string()).collect())
                .unwrap_or_default();
            if !filters.iter().any(|(all, any)| tags_match(&tags, all, any)) {
                continue;
            }
            let tf = world_transform(gt, t, parent);
            entities.push(SnapEntity {
                guid: g.0,
                name: String::new(),
                position: tf.translation.to_array(),
                rotation: tf.rotation.to_array(),
                scale: tf.scale.to_array(),
                tags,
                spline: spline.map(|s| Arc::new(crate::data::SplineData::new(world_spline(s, &tf)))),
                shape: shape.map(|s| world_shape(s, &tf)),
                road_profile: road_of.map(|r| r.profile.clone()),
                mesh_bounds: renderer.and_then(|r| {
                    *mesh_bounds
                        .entry(Arc::as_ptr(&r.mesh) as usize)
                        .or_insert_with(|| vertex_bounds(r.mesh.vertices.iter().map(|v| v.position)))
                }),
            });
        }
        entities.sort_by_key(|e| e.guid);
        for (all, any) in filters {
            let matched = entities
                .iter()
                .filter(|e| tags_match(&e.tags, &all, &any))
                .map(|e| e.guid)
                .collect();
            deps.push(ProcDependency::Query {
                tags: all,
                any_tags: any,
                matched,
            });
        }
        deps.extend(entities.iter().map(|e| ProcDependency::Entity { guid: e.guid }));
    }
    let mut exports = BTreeMap::new();
    let reads: Vec<(String, String)> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.generator_output")
        .map(|n| {
            (
                n.settings.text("generator").to_string(),
                n.settings.text("table").to_string(),
            )
        })
        .collect();
    for (key, export) in reads {
        let Some(src) = find_generator(world, &key) else {
            continue;
        };
        if src == entity {
            continue; // a generator never reads its own output
        }
        let src_guid = world.get::<EntityGuid>(src).map_or(Guid::NIL, |g| g.0);
        let hash = world.get::<ProcGenState>(src).and_then(|s| s.output_hash);
        deps.push(ProcDependency::Generator {
            guid: src_guid,
            output_hash: hash,
        });
        if let Some(out) = world.get::<ProcOutput>(src) {
            let item = match (out.0.tables.get(&export), out.0.points.get(&export)) {
                (Some(t), _) => Some(crate::data::DataItem::new(crate::data::Payload::Table(t.clone()))),
                (None, Some(p)) => Some(crate::data::DataItem::points(p.clone())),
                _ => None,
            };
            if let Some(item) = item {
                exports.insert((key.clone(), export.clone()), item);
            }
        }
    }
    // Data tables for `in.table`, read untyped (spec 63 § 3); a table that does not parse is left
    // out (the node answers `table_missing`) and logged.
    let mut tables = BTreeMap::new();
    let table_paths: Vec<String> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.table")
        .map(|n| n.settings.text("asset").to_string())
        .collect();
    for path in table_paths {
        let Some(text) = read_graph_text(world, &path) else {
            continue;
        };
        match crate::data::table_from_ron(&text) {
            Ok(t) => {
                tables.insert(path.clone(), t);
            }
            Err(e) => tracing::warn!(target: "regia_procgen", "data table `{path}`: {e}"),
        }
        deps.push(ProcDependency::File { path });
    }
    // Mesh geometry for `in.mesh` / `sample.mesh_surface`, from the host's mesh library: the meshes
    // `in.mesh` names and the meshes `spawn.mesh` entries may write onto points.
    let mut meshes = BTreeMap::new();
    if uses(&["in.mesh", "sample.mesh_surface", "spawn.spline_mesh"])
        && let Some(lib) = world.get_resource::<crate::draw::ProcMeshLibrary>()
    {
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        for n in &compiled.nodes {
            match n.kind.id() {
                "in.mesh" => {
                    wanted.insert(n.settings.text("asset").to_string());
                }
                // Module meshes `spawn.spline_mesh` bends (spec 64 § 6).
                "spline.modules" => {
                    if let Some(SettingValue::Map(entries)) = n.settings.get("modules") {
                        wanted.extend(
                            entries
                                .iter()
                                .filter_map(|(_, d)| d.field("mesh").and_then(SettingValue::as_text))
                                .map(str::to_string),
                        );
                    }
                }
                "spawn.mesh" => {
                    if let Some(items) = n.settings.get("entries").and_then(SettingValue::as_items) {
                        wanted.extend(
                            items
                                .iter()
                                .filter_map(|e| e.field("mesh").and_then(SettingValue::as_text))
                                .map(str::to_string),
                        );
                    }
                }
                _ => {}
            }
        }
        for path in wanted {
            if let Some((mesh, _)) = lib.0.get(&path) {
                meshes.insert(
                    path.clone(),
                    Arc::new(crate::data::MeshData {
                        asset: path.clone(),
                        positions: mesh.vertices.iter().map(|v| v.position).collect(),
                        normals: mesh.vertices.iter().map(|v| v.normal).collect(),
                        indices: mesh.indices.clone(),
                    }),
                );
            }
        }
    }
    // Mask pixels for `in.mask` (spec 63 § 3): the host's copy, else a PNG in the project.
    let mut masks = BTreeMap::new();
    let mask_paths: BTreeSet<String> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.mask")
        .map(|n| n.settings.text("asset").to_string())
        .filter(|p| !p.is_empty())
        .collect();
    for path in mask_paths {
        if let Some(image) = world
            .get_resource::<ProcMaskLibrary>()
            .and_then(|lib| lib.0.get(&path))
        {
            masks.insert(path, image.clone());
            continue;
        }
        let bytes = world
            .get_resource::<ProcGenSettings>()
            .and_then(|settings| settings.source.read_bytes(&path));
        if let Some(bytes) = bytes {
            match crate::exec::MaskImage::from_png(&bytes) {
                Ok(image) => {
                    masks.insert(path.clone(), image);
                }
                // The node answers `mask_missing`.
                Err(e) => tracing::warn!(target: "regia_procgen", "mask `{path}`: {e}"),
            }
        }
        deps.push(ProcDependency::File { path });
    }
    // Road profiles the road nodes name in their `profile` setting (spec 64 § 7.1).
    let mut road_profiles = BTreeMap::new();
    let profile_paths: BTreeSet<String> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id().starts_with("road.") || n.kind.id() == "terrain.flatten_road")
        .map(|n| n.settings.text("profile").to_string())
        .chain(entities.iter().filter_map(|e| e.road_profile.clone()))
        .filter(|p| !p.is_empty())
        .collect();
    for path in profile_paths {
        let Some(text) = read_graph_text(world, &path) else {
            continue;
        };
        match crate::road::RoadProfile::from_ron(&text) {
            Ok(p) => {
                road_profiles.insert(path.clone(), p);
            }
            Err(e) => tracing::warn!(target: "regia_procgen", "road profile `{path}`: {e}"),
        }
        deps.push(ProcDependency::File { path });
    }
    // The meshes of the profiles' edge pieces (`road.pieces`, ADR-2731).
    if let Some(lib) = world
        .get_resource::<crate::draw::ProcMeshLibrary>()
        .filter(|_| uses(&["road.pieces"]))
    {
        let inline: Vec<crate::road::RoadProfile> = compiled
            .nodes
            .iter()
            .filter(|n| n.kind.id() == "road.pieces")
            .filter_map(|n| n.settings.get("profile"))
            .filter(|v| matches!(v, SettingValue::Record(..)))
            .filter_map(|v| crate::road::RoadProfile::from_value(v).ok())
            .collect();
        for piece in road_profiles
            .values()
            .chain(&inline)
            .flat_map(|p: &crate::road::RoadProfile| &p.pieces)
        {
            if !meshes.contains_key(&piece.mesh)
                && let Some((mesh, _)) = lib.0.get(&piece.mesh)
            {
                meshes.insert(
                    piece.mesh.clone(),
                    Arc::new(crate::data::MeshData {
                        asset: piece.mesh.clone(),
                        positions: mesh.vertices.iter().map(|v| v.position).collect(),
                        normals: mesh.vertices.iter().map(|v| v.normal).collect(),
                        indices: mesh.indices.clone(),
                    }),
                );
            }
        }
    }
    // Biome areas, exclusions and water (spec 65), and the geometry of the meshes biome entries
    // spawn (their bounds are the default footprints).
    let biome = if compiled.nodes.iter().any(|n| n.kind.id().starts_with("biome.")) {
        let (bw, problems) = crate::biome::ecs::capture_biomes(world, rect, &mut deps);
        for p in &problems {
            tracing::warn!(target: "regia_procgen", "biome area {}: {}: {}", p.guid, p.code, p.message);
        }
        if let Some(lib) = world.get_resource::<crate::draw::ProcMeshLibrary>() {
            for def in bw.defs.values() {
                for e in def.layers.iter().flat_map(|l| &l.entries) {
                    let crate::biome::EntrySpawn::Mesh(path) = &e.spawn else {
                        continue;
                    };
                    if meshes.contains_key(path) {
                        continue;
                    }
                    if let Some((mesh, _)) = lib.0.get(path) {
                        meshes.insert(
                            path.clone(),
                            Arc::new(crate::data::MeshData {
                                asset: path.clone(),
                                positions: mesh.vertices.iter().map(|v| v.position).collect(),
                                normals: mesh.vertices.iter().map(|v| v.normal).collect(),
                                indices: mesh.indices.clone(),
                            }),
                        );
                    }
                }
            }
        }
        Some(Arc::new(bw))
    } else {
        None
    };
    let rays = capture_rays(world, compiled, rect);
    let colliders = capture_colliders(world, compiled, bounds);
    // The entities the rays hit and the colliders the overlaps saw are dependencies: moving or
    // retagging one marks the generator stale (audit M3). A collider added later, that no ray or
    // overlap saw, is not tracked yet.
    let physics: BTreeSet<Guid> = rays
        .values()
        .flatten()
        .map(|h| h.entity.as_str())
        .chain(colliders.values().flatten().map(|c| c.entity.as_str()))
        .filter_map(|g| g.parse::<Guid>().ok())
        .collect();
    deps.extend(physics.into_iter().map(|guid| ProcDependency::Entity { guid }));
    let snap = StaticSnapshot {
        self_info: Some(SelfInfo { guid, name, bounds }),
        ground,
        entities,
        tables,
        exports,
        masks,
        meshes,
        rays,
        colliders,
        painted: world
            .get::<crate::painted::PaintedPoints>(entity)
            .filter(|p| !p.points.is_empty())
            .map(crate::painted::PaintedPoints::to_point_set),
        road_profiles,
        biome,
        self_items: self_items(world, entity),
        content_hash_memo: Default::default(),
    };
    (snap, deps)
}

/// `in.self`'s items beyond the bounds: the entity's `Shape` and `Spline` in world space, a closed
/// spline also as a polygon, each tagged `self` with the generator's scope.
fn self_items(world: &World, entity: Entity) -> Vec<crate::data::DataItem> {
    use crate::data::{AttrValue, DataItem, Payload, SplineData};
    let tf = world_transform(
        world.get::<GlobalTransform>(entity),
        world.get::<Transform>(entity),
        world.get::<ChildOf>(entity).is_some(),
    );
    let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    let name = world
        .get::<Name>(entity)
        .map(|n| n.as_str().to_string())
        .unwrap_or_default();
    let tagged = |payload: Payload| {
        let mut item = DataItem::new(payload);
        item.tags.insert("self".to_string());
        item.scope
            .insert("guid".to_string(), AttrValue::Name(guid.to_string()));
        item.scope
            .insert("name".to_string(), AttrValue::Name(name.clone()));
        item
    };
    let mut out = Vec::new();
    if let Some(s) = world.get::<regia_scene::shape::Shape>(entity) {
        out.push(tagged(Payload::Shape(world_shape(s, &tf))));
    }
    if let Some(s) = world.get::<regia_scene::spline::Spline>(entity) {
        let data = SplineData::new(world_spline(s, &tf));
        if s.closed {
            let ring: Vec<[f64; 2]> = data
                .curve()
                .to_polyline(0.1)
                .iter()
                .map(|p| [f64::from(p.x), f64::from(p.z)])
                .collect();
            let y = f64::from(tf.translation.y);
            match crate::polygon::PolygonData::new([0.0, y, 0.0], &[ring]) {
                Ok(poly) => out.push(tagged(Payload::Polygon(Arc::new(poly)))),
                Err(e) => {
                    tracing::warn!(target: "regia_procgen", code = e.code(), "the generator's closed spline is not a polygon: {e}")
                }
            }
        }
        out.push(tagged(Payload::Spline(Arc::new(data))));
    }
    out
}

/// A compile failure: code, message and the compile error list.
pub type CompileFailure = (String, String, Vec<GraphError>);

/// Casts the downward ray grids of the graph's `in.rays` nodes over the run rect (world-aligned cell
/// centers), against the live physics world. No physics world → no hits.
fn capture_rays(
    world: &mut World,
    compiled: &CompiledGraph,
    rect: [f32; 4],
) -> BTreeMap<u32, Vec<crate::exec::RayHitRecord>> {
    let mut out = BTreeMap::new();
    let nodes: Vec<(u32, crate::nodes::Settings)> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.rays")
        .map(|n| (n.id, n.settings.clone()))
        .collect();
    if nodes.is_empty() || world.get_resource::<regia_physics::PhysicsWorld>().is_none() {
        return out;
    }
    let generated: Vec<Entity> = world
        .query_filtered::<Entity, With<Generated>>()
        .iter(world)
        .collect();
    let world: &World = world;
    let queries = regia_physics::WorldQueries {
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: world.get_resource::<regia_physics::PhysicsLayers>(),
    };
    for (id, s) in nodes {
        let spacing = s.f("spacing_m");
        let (from, max) = (s.f("from_height_m") as f32, s.f("max_dist_m") as f32);
        let layers = s.i("layers");
        let skip = s.b("skip_generated");
        let filter = || {
            let mut f = regia_physics::QueryFilter::new();
            if layers >= 0 {
                f = f.layers(layers as u32);
            }
            if skip {
                f = f.exclude_entities(&generated);
            }
            f
        };
        let ((x0, x1), (z0, z1), count) = crate::nodes::ray_grid(rect, spacing);
        if count > crate::nodes::MAX_RAYS {
            // The node fails the run with `limit_exceeded` naming the count (audit M4).
            tracing::warn!(target: "regia_procgen", "in.rays node {id}: {count} rays is over the cap; none cast");
            continue;
        }
        let mut hits = Vec::new();
        for iz in z0..=z1 {
            for ix in x0..=x1 {
                let (x, z) = (
                    ((ix as f64 + 0.5) * spacing) as f32,
                    ((iz as f64 + 0.5) * spacing) as f32,
                );
                if let Some(h) = queries.raycast(Vec3::new(x, from, z), Vec3::NEG_Y, max, filter()) {
                    hits.push(crate::exec::RayHitRecord {
                        point: h.point.to_array(),
                        normal: h.normal.to_array(),
                        entity: world
                            .get::<EntityGuid>(h.entity)
                            .map(|g| g.0.to_string())
                            .unwrap_or_default(),
                        phys_material: world
                            .get::<regia_physics::PhysicsMaterialRef>(h.entity)
                            .map(|m| m.0.clone())
                            .unwrap_or_default(),
                    });
                }
            }
        }
        out.insert(id, hits);
    }
    out
}

/// Local AABB `(min, max)` of a collider shape (axis-aligned in the collider's frame).
fn shape_bounds(shape: &regia_physics::ColliderShape) -> Option<(Vec3, Vec3)> {
    use regia_physics::{ColliderShape as S, ShapeAxis};
    let along = |axis: ShapeAxis, half_len: f32, r: f32| {
        let mut h = Vec3::splat(r);
        match axis {
            ShapeAxis::X => h.x = half_len,
            ShapeAxis::Y => h.y = half_len,
            ShapeAxis::Z => h.z = half_len,
        }
        Some((-h, h))
    };
    let of_points = |pts: &mut dyn Iterator<Item = Vec3>| {
        let first = pts.next()?;
        Some(pts.fold((first, first), |(lo, hi), p| (lo.min(p), hi.max(p))))
    };
    match shape {
        S::Box { half_extents } => Some((-*half_extents, *half_extents)),
        S::Sphere { radius } => Some((Vec3::splat(-radius), Vec3::splat(*radius))),
        S::Capsule {
            half_height,
            radius,
            axis,
        } => along(*axis, half_height + radius, *radius),
        S::Cylinder {
            half_height,
            radius,
            axis,
        }
        | S::Cone {
            half_height,
            radius,
            axis,
        } => along(*axis, *half_height, *radius),
        S::ConvexHull { points } => of_points(&mut points.iter().copied()),
        S::TriMesh { vertices, .. } => of_points(&mut vertices.iter().copied()),
        S::HeightField { heights, scale, .. } => {
            let (lo, hi) = heights
                .iter()
                .filter(|h| h.is_finite())
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), h| {
                    (a.min(*h), b.max(*h))
                });
            (lo <= hi).then(|| {
                (
                    Vec3::new(-0.5 * scale.x, lo * scale.y, -0.5 * scale.z),
                    Vec3::new(0.5 * scale.x, hi * scale.y, 0.5 * scale.z),
                )
            })
        }
        S::Compound { shapes } => {
            let mut acc: Option<(Vec3, Vec3)> = None;
            for (tf, child) in shapes {
                let Some((lo, hi)) = shape_bounds(child) else {
                    continue;
                };
                let (wlo, whi) = transform_aabb(tf, lo, hi);
                acc = Some(acc.map_or((wlo, whi), |(a, b)| (a.min(wlo), b.max(whi))));
            }
            acc
        }
    }
}

/// An AABB through a transform (the bounds of the transformed box). Rotation goes through the
/// scalar [`crate::rot::Rot`], not glam's quaternion math, whose last bits differ between targets
/// (`rot.rs`; audit L15: an `in.overlap` box decides keep or drop at its edge).
fn transform_aabb(tf: &Transform, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
    let r = crate::rot::Rot(tf.rotation.to_array());
    let m = r.to_mat3();
    let (c, h) = ((lo + hi) * 0.5 * tf.scale, (hi - lo) * 0.5 * tf.scale.abs());
    let ext = Vec3::new(
        m.x_axis.x.abs() * h.x + m.y_axis.x.abs() * h.y + m.z_axis.x.abs() * h.z,
        m.x_axis.y.abs() * h.x + m.y_axis.y.abs() * h.y + m.z_axis.y.abs() * h.z,
        m.x_axis.z.abs() * h.x + m.y_axis.z.abs() * h.y + m.z_axis.z.abs() * h.z,
    );
    let center = tf.translation + r * c;
    (center - ext, center + ext)
}

/// Most mesh triangles one collider contributes as exact parts; a larger mesh is tested as its AABB.
const MAX_COLLIDER_TRIANGLES: usize = 65_536;

/// `p` through `chain` (innermost first): each `translation + rotation * (scale * p)`.
fn chain_point(chain: &[Transform], p: Vec3) -> Vec3 {
    chain.iter().fold(p, |p, tf| {
        tf.translation + crate::rot::Rot(tf.rotation.to_array()) * (tf.scale * p)
    })
}

/// A direction through `chain` (rotations only), normalized.
fn chain_dir(chain: &[Transform], d: Vec3) -> Vec3 {
    chain
        .iter()
        .fold(d, |d, tf| crate::rot::Rot(tf.rotation.to_array()) * d)
        .normalize_or_zero()
}

/// The largest absolute scale along `chain` (radii grow by it: exact for uniform scale).
fn chain_scale(chain: &[Transform]) -> f32 {
    chain.iter().map(|tf| tf.scale.abs().max_element()).product()
}

/// One part from local points, disks and a radius through `chain`, with its world AABB.
fn make_part(
    chain: &[Transform],
    points: &[Vec3],
    disks: &[(Vec3, Vec3, f32)],
    radius: f32,
) -> crate::exec::ConvexPart {
    let k = chain_scale(chain);
    let points: Vec<[f32; 3]> = points.iter().map(|p| chain_point(chain, *p).to_array()).collect();
    let disks: Vec<([f32; 3], [f32; 3], f32)> = disks
        .iter()
        .map(|(c, n, r)| {
            (
                chain_point(chain, *c).to_array(),
                chain_dir(chain, *n).to_array(),
                r * k,
            )
        })
        .collect();
    let radius = radius * k;
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for p in &points {
        lo = lo.min(Vec3::from(*p));
        hi = hi.max(Vec3::from(*p));
    }
    for (c, _, r) in &disks {
        lo = lo.min(Vec3::from(*c) - Vec3::splat(*r));
        hi = hi.max(Vec3::from(*c) + Vec3::splat(*r));
    }
    crate::exec::ConvexPart {
        points,
        disks,
        radius,
        min: (lo - Vec3::splat(radius)).to_array(),
        max: (hi + Vec3::splat(radius)).to_array(),
    }
}

/// The convex parts of a collider shape in world space (`chain` = shape-to-world, innermost first).
/// Height fields, and meshes over `MAX_COLLIDER_TRIANGLES`, give their AABB as a box part.
fn shape_parts(
    shape: &regia_physics::ColliderShape,
    chain: &[Transform],
    out: &mut Vec<crate::exec::ConvexPart>,
) {
    use regia_physics::{ColliderShape as S, ShapeAxis};
    let axis = |a: ShapeAxis| match a {
        ShapeAxis::X => Vec3::X,
        ShapeAxis::Y => Vec3::Y,
        ShapeAxis::Z => Vec3::Z,
    };
    let corners = |lo: Vec3, hi: Vec3| -> Vec<Vec3> {
        (0..8)
            .map(|i| {
                Vec3::new(
                    if i & 1 == 0 { lo.x } else { hi.x },
                    if i & 2 == 0 { lo.y } else { hi.y },
                    if i & 4 == 0 { lo.z } else { hi.z },
                )
            })
            .collect()
    };
    match shape {
        S::Box { half_extents } => out.push(make_part(
            chain,
            &corners(-*half_extents, *half_extents),
            &[],
            0.0,
        )),
        S::Sphere { radius } => out.push(make_part(chain, &[Vec3::ZERO], &[], *radius)),
        S::Capsule {
            half_height,
            radius,
            axis: a,
        } => {
            let h = axis(*a) * *half_height;
            out.push(make_part(chain, &[-h, h], &[], *radius));
        }
        S::Cylinder {
            half_height,
            radius,
            axis: a,
        } => {
            let n = axis(*a);
            let h = n * *half_height;
            out.push(make_part(chain, &[], &[(-h, n, *radius), (h, n, *radius)], 0.0));
        }
        S::Cone {
            half_height,
            radius,
            axis: a,
        } => {
            // Apex at +half_height along the axis, base disk at -half_height.
            let n = axis(*a);
            let h = n * *half_height;
            out.push(make_part(chain, &[h], &[(-h, n, *radius)], 0.0));
        }
        S::ConvexHull { points } => {
            // The physics builds no collider from a hull with fewer than three points
            // (step.rs); the capture must not invent a part for it either — an empty hull's
            // AABB is inverted (+inf < −inf) and a near-empty one can confuse the exact test.
            if points.len() >= 3 {
                out.push(make_part(chain, points, &[], 0.0));
            }
        }
        S::TriMesh { vertices, indices } if indices.len() <= MAX_COLLIDER_TRIANGLES => {
            for tri in indices {
                let pts: Option<Vec<Vec3>> = tri.iter().map(|&i| vertices.get(i as usize).copied()).collect();
                if let Some(pts) = pts {
                    out.push(make_part(chain, &pts, &[], 0.0));
                }
            }
        }
        S::Compound { shapes } => {
            for (tf, child) in shapes {
                let mut inner = Vec::with_capacity(chain.len() + 1);
                inner.push(*tf);
                inner.extend_from_slice(chain);
                shape_parts(child, &inner, out);
            }
        }
        S::TriMesh { .. } | S::HeightField { .. } => {
            if let Some((lo, hi)) = shape_bounds(shape) {
                out.push(make_part(chain, &corners(lo, hi), &[], 0.0));
            }
        }
    }
}

/// Local bounds `(min, max)`.
type LocalBounds = ([f32; 3], [f32; 3]);

/// The bounds `(min, max)` of finite positions; `None` when there are none.
fn vertex_bounds(positions: impl Iterator<Item = [f32; 3]>) -> Option<LocalBounds> {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for p in positions.filter(|p| p.iter().all(|v| v.is_finite())) {
        lo = std::array::from_fn(|k| lo[k].min(p[k]));
        hi = std::array::from_fn(|k| hi[k].max(p[k]));
    }
    (lo[0] <= hi[0]).then_some((lo, hi))
}

/// Captures the world AABBs of the colliders touching the generator bounds for the graph's
/// `in.overlap` nodes (an overlap query with the bounds box, then each collider's shape bounds
/// through its transform and offset).
fn capture_colliders(
    world: &mut World,
    compiled: &CompiledGraph,
    bounds: ShapeData,
) -> BTreeMap<u32, Vec<crate::exec::ColliderBox>> {
    let mut out = BTreeMap::new();
    let nodes: Vec<(u32, i64, bool)> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.overlap")
        .map(|n| (n.id, n.settings.i("layers"), n.settings.b("skip_generated")))
        .collect();
    if nodes.is_empty() || world.get_resource::<regia_physics::PhysicsWorld>().is_none() {
        return out;
    }
    let generated: Vec<Entity> = world
        .query_filtered::<Entity, With<Generated>>()
        .iter(world)
        .collect();
    let world: &World = world;
    let queries = regia_physics::WorldQueries {
        world: world.resource::<regia_physics::PhysicsWorld>(),
        layers: world.get_resource::<regia_physics::PhysicsLayers>(),
    };
    let (lo, hi) = bounds.world_aabb();
    let half = Vec3::from(hi) - Vec3::from(lo);
    let pose = Transform::from_translation((Vec3::from(lo) + Vec3::from(hi)) * 0.5);
    let probe = regia_physics::ColliderShape::Box {
        half_extents: half * 0.5,
    };
    for (id, layers, skip) in nodes {
        let mut filter = regia_physics::QueryFilter::new();
        if layers >= 0 {
            filter = filter.layers(layers as u32);
        }
        if skip {
            filter = filter.exclude_entities(&generated);
        }
        let mut hits = queries.overlap(&probe, pose, filter);
        hits.sort();
        hits.dedup();
        let mut boxes = Vec::new();
        for e in hits {
            let Some(c) = world.get::<regia_physics::Collider>(e) else {
                continue;
            };
            let Some((slo, shi)) = shape_bounds(&c.shape) else {
                continue;
            };
            let parent = world_transform(
                world.get::<GlobalTransform>(e),
                world.get::<Transform>(e),
                world.get::<ChildOf>(e).is_some(),
            );
            let (llo, lhi) = transform_aabb(&c.offset, slo, shi);
            let (wlo, whi) = transform_aabb(&parent, llo, lhi);
            let mut parts = Vec::new();
            shape_parts(&c.shape, &[c.offset, parent], &mut parts);
            boxes.push(crate::exec::ColliderBox {
                min: wlo.to_array(),
                max: whi.to_array(),
                entity: world
                    .get::<EntityGuid>(e)
                    .map(|g| g.0.to_string())
                    .unwrap_or_default(),
                parts,
            });
        }
        boxes.sort_by(|a, b| a.entity.cmp(&b.entity).then(a.min[0].total_cmp(&b.min[0])));
        out.insert(id, boxes);
    }
    out
}

/// Compiles the generator's graph with its variant chain and overrides; also returns the files read.
///
/// # Errors
///
/// `procgen_not_generator`, the load error code (`procgen_unknown_graph`, `variant_cycle`, ...),
/// `procgen_param_type` or `procgen_graph_invalid` with the compile errors (including
/// `override_path_unknown` / `override_type_mismatch` for `spawn.prefab` override paths that do not
/// match the registered reflected types).
pub fn compile_for(world: &World, entity: Entity) -> Result<(CompiledGraph, Vec<String>), CompileFailure> {
    compile_for_tracked(world, entity).0
}

/// As [`compile_for`], also returning every path the compile's loaders asked for, whether or not it
/// could be read. On a failure this is what an anchored host watches: a called graph, variant or
/// base file that did not exist yet, or a new subgraph that did not parse (fixrev5-procgen-missing-calls).
pub(crate) fn compile_for_tracked(world: &World, entity: Entity) -> (CompileResult, BTreeSet<String>) {
    let asked = std::cell::RefCell::new(BTreeSet::new());
    let load = |p: &str| {
        asked.borrow_mut().insert(p.to_string());
        read_graph_text(world, p)
    };
    let result = compile_with_loader(world, entity, &load);
    (result, asked.into_inner())
}

/// What [`compile_for`] returns.
type CompileResult = Result<(CompiledGraph, Vec<String>), CompileFailure>;

fn compile_with_loader(
    world: &World,
    entity: Entity,
    load: &dyn Fn(&str) -> Option<String>,
) -> CompileResult {
    let Some(gen_c) = world.get::<ProcGenerator>(entity) else {
        return Err((
            "procgen_not_generator".into(),
            "not a generator".into(),
            Vec::new(),
        ));
    };
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    let resolved = resolve_graph(&gen_c.graph, load).map_err(|e| (e.code, e.message, Vec::new()))?;
    // Plant graphs grow assets in the pipeline; a level generator never runs one (spec 68 § 2).
    if resolved.graph.flags.domain == crate::nodes::Domain::Plant {
        let code = if cfg!(feature = "pipeline") {
            "procgen_wrong_domain"
        } else {
            "procgen_plant_runtime_forbidden"
        };
        return Err((
            code.to_string(),
            format!(
                "`{}` is a Plant graph: run it with `regia procgen plant`",
                gen_c.graph
            ),
            Vec::new(),
        ));
    }
    let params = gen_c.parsed_params().map_err(|k| {
        (
            "procgen_param_type".to_string(),
            format!("parameter `{k}` is not RON"),
            Vec::new(),
        )
    })?;
    let merged = resolved.merged_with(&params);
    let compiled = resolved
        .graph
        .compile_full(&registry, &merged, load)
        .map_err(|e| ("procgen_graph_invalid".to_string(), e.to_string(), e.0))?;
    // `spawn.prefab` override paths against the reflected types (ADR-3027).
    if let Some(types) = world.get_resource::<regia_ecs::reflect::AppTypeRegistry>() {
        let errors = crate::prefab_overrides::check_compiled_overrides(&compiled, &types.read());
        if !errors.is_empty() {
            let e = crate::graph::GraphErrors(errors);
            return Err(("procgen_graph_invalid".to_string(), e.to_string(), e.0));
        }
    }
    let mut files = resolved.files;
    files.extend(crate::graph::collect_subgraphs_with(&resolved.graph, &merged, load).into_keys());
    Ok((compiled, files))
}

fn fail(world: &mut World, entity: Entity, run_id: u64, node: Option<u32>, code: &str, message: String) {
    if let Some(mut st) = state_mut(world, entity) {
        st.state = ProcGenRunState::Failed;
        st.job = None;
        st.last_error = Some((node, code.to_string(), message));
    }
    world.write_message(ProcGenFailed {
        entity,
        run_id,
        node,
        code: code.to_string(),
    });
    log(world, entity, "failed", run_id);
}

/// The texts of the subgraphs a run may call (every `.pgraph.ron` file the compile touched).
fn subgraph_texts(world: &World, files: &[String]) -> BTreeMap<String, String> {
    files
        .iter()
        .filter(|p| !p.ends_with(".pgvar.ron"))
        .filter_map(|p| read_graph_text(world, p).map(|t| (p.clone(), t)))
        .collect()
}

/// How a run was asked for.
#[derive(Clone, Copy, Debug, Default)]
struct RunAsk {
    /// A bake or verify (tools): `editor_only` generators run.
    tool: bool,
    /// The tier to run at instead of the generator's (a `PerTier` bake).
    tier: Option<QualityTier>,
}

/// The tier a run uses (spec 62 § 7.4): a `Baked` generator its `quality_tier` (default High), a
/// `PerTier` one, `OnLoad` and `Anchored` the running tier.
pub fn run_quality(generator: &ProcGenerator, settings: &ProcGenSettings) -> QualityTier {
    match (generator.mode, generator.quality_tier) {
        (ProcGenMode::Baked, Some(ProcBakeTier::PerTier)) => settings.running_tier,
        (ProcGenMode::Baked, t) => t.unwrap_or_default().tier(),
        _ => settings.running_tier,
    }
}

/// The content hash of a file a run reads: the unsaved overlay text, the memory source, or the file's
/// bytes under the project; `None` when it cannot be read.
pub fn file_content_hash(world: &World, path: &str) -> Option<[u8; 32]> {
    if let Some(t) = world
        .get_resource::<ProcGraphOverlay>()
        .and_then(|o| o.0.get(path))
    {
        return Some(*blake3::hash(t.as_bytes()).as_bytes());
    }
    match &world.get_resource::<ProcGenSettings>()?.source {
        ProcGraphSource::Memory(files) => files.get(path).map(|t| *blake3::hash(t.as_bytes()).as_bytes()),
        ProcGraphSource::Dir(root) => regia_core::project_path::resolve_read(root, path)
            .ok()
            .and_then(|f| std::fs::read(f).ok())
            .map(|b| *blake3::hash(&b).as_bytes()),
    }
}

fn file_hashes_of(world: &World, deps: &[ProcDependency]) -> BTreeMap<String, Option<[u8; 32]>> {
    deps.iter()
        .filter_map(|d| match d {
            ProcDependency::File { path } => Some(path),
            _ => None,
        })
        .map(|p| (p.clone(), file_content_hash(world, p)))
        .collect()
}

/// Starts a queued run: capture, compile, `Started`, and hands evaluation to a worker (or to the
/// next update when inline). Returns the run id.
fn start(world: &mut World, entity: Entity) -> Option<u64> {
    start_with(world, entity, RunAsk::default())
}

fn start_with(world: &mut World, entity: Entity, ask: RunAsk) -> Option<u64> {
    let _span = tracing::info_span!("procgen.capture").entered();
    // Anchored generators with runtime settings run per cell in `ProcGenSet::Schedule` (baked ones
    // load per cell); a run request re-runs or reloads their resident cells (spec 66 § 4.2a).
    if world
        .get::<ProcGenerator>(entity)
        .is_some_and(crate::tiles::is_scheduled)
    {
        // No whole-graph run id and no worker. A leftover `Queued` state would stay in flight
        // and block generators that read this one.
        requeue_scheduled(world, entity);
        return None;
    }
    let run_id = {
        let mut st = state_mut(world, entity)?;
        st.run_id += 1;
        st.state = ProcGenRunState::Capturing;
        st.run_id
    };
    world.write_message(ProcGenStarted { entity, run_id });
    log(world, entity, "started", run_id);
    if !ask.tool && editor_only_refused(world, entity) {
        fail(
            world,
            entity,
            run_id,
            None,
            "procgen_editor_only",
            "the generator is editor_only: it runs only in the editor and in bakes".to_string(),
        );
        return Some(run_id);
    }
    if let Some(message) = bounds_problem(world, entity) {
        fail(world, entity, run_id, None, "procgen_bounds_missing", message);
        return Some(run_id);
    }
    let (compiled, files) = match compile_for(world, entity) {
        Ok(c) => c,
        Err((code, message, _)) => {
            fail(world, entity, run_id, None, &code, message);
            return Some(run_id);
        }
    };
    let (snap, mut deps) = capture(world, entity, &compiled);
    let graphs = Arc::new(subgraph_texts(world, &files));
    deps.extend(files.into_iter().map(|path| ProcDependency::File { path }));
    let settings = world.resource::<ProcGenSettings>().clone();
    let generator = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    let mut limits = RunLimits::for_platform();
    if let Some(l) = generator.limits {
        limits = limits.with_override(&l.to_override());
    }
    let quality = ask.tier.unwrap_or_else(|| run_quality(&generator, &settings));
    let mut tile = match tile_ctx(&generator, &compiled, generator_bounds(world, entity)) {
        Ok(t) => t,
        Err((code, message)) => {
            fail(world, entity, run_id, None, &code, message);
            return Some(run_id);
        }
    };
    if let Some(t) = &mut tile {
        let key = (generator_seed(world, entity), quality);
        t.partial = partial_run(
            world,
            entity,
            t,
            &compiled,
            (compiled.graph_hash, compiled.param_hash),
            key,
        );
    }
    let inputs = RunInputs {
        world: Arc::new(snap),
        gen_seed: generator_seed(world, entity),
        tier_index: 0,
        cell: None,
        quality,
        limits,
        keep_debug: settings.keep_debug,
        graphs,
        terrain_edits: terrain_edit_gate(&generator, &settings),
        run_rect: None,
        tile: None,
    };
    let cancel_token = CancelToken::new();
    let (graph_hash, param_hash) = (compiled.graph_hash, compiled.param_hash);
    let pending = if settings.inline {
        JobState::Inline(Box::new((compiled, inputs, tile)))
    } else {
        let (tx, rx) = std::sync::mpsc::channel();
        let cache = world.resource::<ProcGenCache>().0.clone();
        let token = cancel_token.clone();
        let threads = settings.threads;
        regia_core::tasks::spawn(async move {
            let r = evaluate(
                &run_evaluator(cache, threads),
                &compiled,
                &inputs,
                tile.as_ref(),
                &token,
            );
            // The receiver may be gone (generator despawned): nothing to deliver to.
            let _ = tx.send(r);
        })
        .detach();
        JobState::Thread(Mutex::new(rx))
    };
    let file_hashes = file_hashes_of(world, &deps);
    let run_bounds = generator_bounds(world, entity);
    let mut st = state_mut(world, entity)?;
    st.deps = deps;
    st.file_hashes = file_hashes;
    st.run_bounds = Some(run_bounds);
    st.stale = false;
    st.stale_frames = 0;
    st.state = ProcGenRunState::Running;
    st.job = Some(Job {
        run_id,
        cancel: cancel_token,
        pending,
        graph_hash,
        param_hash,
    });
    Some(run_id)
}

/// The total order the deterministic captures use (`capture_inner`'s ground pick): guid-less
/// entities sort last, then by guid. ECS query order follows spawn and archetype history, which
/// a save/load cycle changes, so anything two runs race on resolves through this key.
fn guid_key(world: &World, entity: Entity) -> (bool, Guid) {
    match world.get::<EntityGuid>(entity) {
        Some(g) => (false, g.0),
        None => (true, Guid::NIL),
    }
}

/// Whether `entity`'s run waits behind another generator with a lower guid that is still in
/// flight (see [`poll`]).
fn waits_behind_lower_guid(world: &mut World, entity: Entity) -> bool {
    let own = guid_key(world, entity);
    let others: Vec<Entity> = {
        let mut q = world.query::<(Entity, &ProcGenState)>();
        q.iter(world)
            .filter(|(e, s)| *e != entity && s.job.is_some())
            .map(|(e, _)| e)
            .collect()
    };
    others.iter().any(|&e| guid_key(world, e) < own)
}

/// Publishes a finished run (spec 62 § 7.2 `applying`): the old output is replaced in one call.
fn apply(world: &mut World, entity: Entity, run_id: u64, result: RunResult, hashes: ([u8; 32], [u8; 32])) {
    let _span = tracing::info_span!("procgen.apply", instances = result.output.instance_count()).entered();
    let generator_guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    // Terrain first: edits that do not fit fail the run with the old output still published.
    let refresh = state_mut(world, entity).is_some_and(|mut s| std::mem::take(&mut s.refresh_run));
    let published = if refresh {
        crate::terrain_edits::publish(world, entity, &result.output.terrain_edits)
    } else {
        crate::terrain_edits::logged(world, entity, |w| {
            crate::terrain_edits::publish(w, entity, &result.output.terrain_edits)
        })
    };
    if let Err((code, message)) = published {
        fail(world, entity, run_id, None, &code, message);
        return;
    }
    despawn_outputs(world, entity);
    let output = Arc::new(result.output);
    let spawned = spawn_prefabs(world, entity, generator_guid, run_id, &output);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(ProcOutput(output.clone()));
        e.remove::<ProcBakeOrigin>();
    }
    let counts = [output.instance_count(), spawned, output.tables.len()];
    if let Some(mut st) = state_mut(world, entity) {
        st.state = ProcGenRunState::Ready;
        st.last_ok_run = run_id;
        st.output_hash = Some(result.output_hash);
        st.report = Some(result.report);
        st.debug = result.debug.map(Arc::new);
        st.last_error = None;
        st.hashes = Some(hashes);
        st.job = None;
        st.stale_cells.clear();
        st.stale_whole = false;
    }
    world.write_message(ProcGenFinished {
        entity,
        run_id,
        output_hash: result.output_hash,
        counts,
    });
    log(world, entity, "finished", run_id);
}

/// Spawns the prefab records of an output under a new output root; returns how many.
fn spawn_prefabs(world: &mut World, entity: Entity, guid: Guid, run_id: u64, output: &RunOutput) -> usize {
    spawn_prefab_root(world, entity, guid, guid, None, run_id, output).0
}

/// The `EntityGuid` of the `index`-th prefab instance of the output identified by `scope` (the
/// generator's guid, or a tile cell's scope): derived, never random, so lockstep peers and a
/// regenerated output name the same instance the same way, and the prefab-internal guids that
/// `instantiate_prefab` derives from it follow (ADR-4806 refuses an instance root without one).
pub fn prefab_instance_guid(scope: Guid, index: usize) -> Guid {
    regia_scene::placed::derive_guid(scope, Guid::from_u128(index as u128 + 1))
}

/// The salt of an output root's derived guid: far above every prefab index + 1, so a root never
/// shares a guid with one of the instances `prefab_instance_guid` names under the same scope.
const OUTPUT_ROOT_SALT: u128 = 0x5052_4f43_4f55_5452_4f4f_5400_0000_0000;

/// The `EntityGuid` of the output root of the generator `owner` (whose guid is `generator`):
/// its whole output's root when `cell` is `None`, else the root of that tile cell. Only a root in
/// a layer gets one, so layer switches reach it (spec 62 § 18). Derived like the prefab instances'
/// guids, so peers and a regenerated output agree on it. A generator without an `EntityGuid` has
/// no guid of its own to derive from: its roots' guids derive from its entity id (index +
/// generation) instead, mixed in before the cell scope, so two such generators' roots never share
/// one guid, whole or tiled (rev-claude-62 note 2; sprint-rev-w9-zai-k finding 1: a cell's scope
/// is never `Guid::NIL`, so mixing after it missed the tiled case). The entity id is stable and
/// unique within a session; nothing persists the root guid (level saves and scene snapshots skip
/// `Generated` entities), so it may differ between sessions.
fn output_root_guid(generator: Guid, cell: Option<crate::tiles::TileKey>, owner: Entity) -> Guid {
    let generator = if generator == Guid::NIL {
        regia_scene::placed::derive_guid(generator, Guid::from_u128(u128::from(owner.to_bits())))
    } else {
        generator
    };
    let scope = cell.map_or(generator, |key| crate::tiles::cell_scope(generator, key));
    regia_scene::placed::derive_guid(scope, Guid::from_u128(OUTPUT_ROOT_SALT))
}

/// Spawns the prefab records of an output under a new output root; returns how many and the root.
/// `scope` names this output for [`prefab_instance_guid`]: the generator's guid for a whole
/// output, a per-cell guid for a tile cell (cells share a generator, so they need distinct scopes).
/// `cell` is that tile cell (`None` for a whole output); the root's own guid derives from it.
pub(crate) fn spawn_prefab_root(
    world: &mut World,
    entity: Entity,
    guid: Guid,
    scope: Guid,
    cell: Option<crate::tiles::TileKey>,
    run_id: u64,
    output: &RunOutput,
) -> (usize, Entity) {
    let marker = Generated {
        generator: guid,
        run_id,
    };
    // A generator without a guid exists only at runtime (it is never saved): its scope mixes in the
    // entity, so two of them do not hand out the same prefab instance guids.
    let scope = if guid == Guid::NIL {
        regia_scene::placed::derive_guid(scope, Guid::from_u128(u128::from(entity.to_bits()) + 1))
    } else {
        scope
    };
    let root = world
        .spawn((
            ProcOutputRoot { generator: entity },
            marker,
            Transform::default(),
            GlobalTransform::default(),
        ))
        .id();
    // The root carries the generator's layer membership, so switching the layer off parks or
    // removes what the generator made (spec 62 § 18, ADR-1585); generated children follow the root.
    let membership = world.get::<regia_scene::layers::LayerMembership>(entity).cloned();
    for (index, p) in output.prefabs.iter().enumerate() {
        let tf = Transform {
            translation: Vec3::from(p.translation),
            rotation: Quat::from_array(p.rotation),
            scale: Vec3::from(p.scale),
        };
        let mut e = world.spawn((
            tf,
            GlobalTransform::default(),
            marker,
            Name::new(p.name.as_str()),
            Tags::from_iter_str(p.tags.iter().map(String::as_str)),
            ProcPrefabRef(p.prefab.clone()),
            EntityGuid(prefab_instance_guid(scope, index)),
            ChildOf(root),
        ));
        if let Some(hex) = p.prefab.strip_prefix("guid:")
            && let Ok(id) = u128::from_str_radix(hex, 16)
        {
            e.insert(regia_scene::prefab::PendingPrefab {
                prefab: id,
                root_transform: Some(tf),
                ..Default::default()
            });
        }
        if !p.overrides.is_empty() {
            e.insert(crate::prefab_overrides::ProcPrefabOverrides {
                prefab: p.prefab.clone(),
                overrides: p.overrides.clone(),
            });
        }
    }
    // Generated meshes that asked for collision (world-space vertices, identity transform).
    for m in output.meshes.iter().filter(|m| m.collision) {
        let Some(shape) = regia_physics::ColliderShape::trimesh_from_triangles(&m.positions, &m.indices)
        else {
            continue;
        };
        world.spawn((
            Transform::default(),
            GlobalTransform::default(),
            marker,
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape,
                ..regia_physics::Collider::default()
            },
            ChildOf(root),
        ));
    }
    // Instance sets that asked for collision (spec 62 § 7.3): one static compound of boxes and one
    // triangle mesh of every `Mesh` instance per output (a generator, or a cell of a tiled one).
    spawn_instance_colliders(world, entity, root, marker, output);
    // Membership last, so the presence check parks the whole subtree at once. The derived guid
    // makes the root a layer member that `apply_layer_switches` parks and unparks at every later
    // switch (it only walks members with a guid); with no template record and no level it is
    // parked, never despawned, for `Absent` (spec 62 § 18).
    if let Some(membership) = membership {
        if let Ok(mut e) = world.get_entity_mut(root) {
            e.insert((membership, EntityGuid(output_root_guid(guid, cell, entity))));
        }
        regia_scene::layers::apply_member_presence(world, root);
    }
    (output.prefabs.len(), root)
}

/// Spawns the instance-set colliders of `output` under `root` and records on the root which mesh
/// library entries they were built from ([`refresh_instance_colliders`] rebuilds them when those
/// change).
fn spawn_instance_colliders(
    world: &mut World,
    generator: Entity,
    root: Entity,
    marker: Generated,
    output: &RunOutput,
) {
    let built = instance_colliders(world, output);
    let meshes = ColliderMeshes::of(world.get_resource::<crate::draw::ProcMeshLibrary>(), output);
    // Colliders rebuilt under a parked root arrive parked, as its other children are.
    let parked = world.get::<regia_scene::layers::LayerParked>(root).is_some();
    for (layer, kind, instances, shape) in built {
        let mut e = world.spawn((
            Transform::default(),
            GlobalTransform::default(),
            marker,
            regia_physics::RigidBody::static_body(),
            regia_physics::Collider {
                shape,
                ..regia_physics::Collider::default()
            },
            ProcInstanceCollider {
                generator,
                kind,
                instances,
            },
            ChildOf(root),
        ));
        // A layered set's collider follows its layer (`follow_generator_layers`).
        if let Some(layer) = layer {
            e.insert(ProcLayerFollow { layer });
        }
        if parked {
            e.insert(regia_scene::layers::LayerParked {
                restore: regia_scene::components::Visibility::default(),
                inherited: true,
            });
        }
    }
    if let Some(meshes) = meshes
        && let Ok(mut e) = world.get_entity_mut(root)
    {
        e.insert(meshes);
    }
}

/// On an output root whose instance sets collide: the mesh library entry each collision mesh had
/// when the colliders were built (`None` = missing, a unit box). A weak handle compares by
/// allocation without keeping the mesh alive, and a dropped mesh can never alias a new one while
/// the handle exists.
#[derive(Component, Debug, Clone)]
struct ColliderMeshes(Vec<(String, Option<std::sync::Weak<regia_render::CpuMesh>>)>);

impl ColliderMeshes {
    /// The entries of `output`'s colliding meshes in `lib`; `None` when nothing in it collides.
    fn of(lib: Option<&crate::draw::ProcMeshLibrary>, output: &RunOutput) -> Option<Self> {
        let names: BTreeSet<&str> = output
            .instance_sets
            .iter()
            .filter(|s| s.collision != crate::exec::Collision::None && !s.instances.is_empty())
            .map(|s| s.mesh.as_str())
            .collect();
        if names.is_empty() {
            return None;
        }
        Some(Self(
            names
                .into_iter()
                .map(|n| {
                    let weak = lib.and_then(|l| l.0.get(n)).map(|(m, _)| Arc::downgrade(m));
                    (n.to_string(), weak)
                })
                .collect(),
        ))
    }

    /// Whether `lib` now holds a different entry (added, removed or replaced) for any of them.
    fn stale(&self, lib: Option<&crate::draw::ProcMeshLibrary>) -> bool {
        self.0.iter().any(|(name, then)| {
            let now = lib.and_then(|l| l.0.get(name)).map(|(m, _)| m);
            match (then, now) {
                (None, None) => false,
                (Some(w), Some(m)) => !std::ptr::eq(w.as_ptr(), Arc::as_ptr(m)),
                _ => true,
            }
        })
    }
}

/// Rebuilds instance-set colliders when the [`crate::draw::ProcMeshLibrary`] they were built from
/// changed (appeared, went away, or gained, lost or replaced one of their meshes), the same
/// invalidation the draw path uses: an output applied before its meshes loaded first collides as
/// unit boxes and gets the real bounds and triangles once the host registers them, without a re-run.
/// Only roots whose colliding meshes changed are rebuilt.
pub fn refresh_instance_colliders(world: &mut World, mut had_library: Local<bool>) {
    let has_library = world.contains_resource::<crate::draw::ProcMeshLibrary>();
    let changed = world.is_resource_changed::<crate::draw::ProcMeshLibrary>() || has_library != *had_library;
    *had_library = has_library;
    if !changed {
        return;
    }
    let _span = tracing::info_span!("procgen.refresh_instance_colliders").entered();
    let mut roots = world.query::<(Entity, &ProcOutputRoot, &Generated, &ColliderMeshes)>();
    let lib = world.get_resource::<crate::draw::ProcMeshLibrary>();
    let mut stale: Vec<(Entity, Entity, Generated)> = roots
        .iter(world)
        .filter(|(_, _, _, m)| m.stale(lib))
        .map(|(root, r, marker, _)| (root, r.generator, *marker))
        .collect();
    stale.sort_by_key(|(root, ..)| *root);
    for (root, generator, marker) in stale {
        // A tile cell's root holds its output; a whole output lives on the generator.
        let Some(output) = world
            .get::<ProcOutput>(root)
            .or_else(|| world.get::<ProcOutput>(generator))
            .map(|o| o.0.clone())
        else {
            continue;
        };
        let old: Vec<Entity> = world
            .get::<Children>(root)
            .map(|c| {
                c.iter()
                    .filter(|&e| world.get::<ProcInstanceCollider>(e).is_some())
                    .collect()
            })
            .unwrap_or_default();
        for e in old {
            if let Ok(e) = world.get_entity_mut(e) {
                e.despawn();
            }
        }
        if let Ok(mut e) = world.get_entity_mut(root) {
            e.remove::<ColliderMeshes>();
        }
        spawn_instance_colliders(world, generator, root, marker, &output);
    }
}

/// Keeps an applied output root's `LayerMembership` in step with its generator's (spec 62 § 18):
/// moving a generator to another layer (or joining one after the apply) moves the applied root,
/// its prefab instances and its colliders too, without waiting for the next apply (the
/// layer-move note of the procgen-runtime-fixes report: the root used to stay behind until a
/// re-run replaced it, so the old layer kept governing the applied output). A generator that
/// leaves every layer keeps its applied root as it was applied: unparking a membership-less root
/// needs `regia_scene`-private machinery, and the next apply rebuilds the root without one.
pub(crate) fn sync_output_root_layers(world: &mut World) {
    let roots: Vec<(Entity, Entity)> = {
        let mut q = world.query::<(Entity, &ProcOutputRoot)>();
        q.iter(world).map(|(e, r)| (e, r.generator)).collect()
    };
    // Roots whose membership differs from their generator's current one.
    let mut out_of_step: Vec<(Entity, Entity, regia_scene::layers::LayerMembership)> = Vec::new();
    for (root, generator) in roots {
        if world.get_entity(generator).is_err() {
            continue; // `despawn_orphan_outputs` takes the root
        }
        // A generator that left every layer keeps its applied root as it was applied: unparking
        // a membership-less root needs `regia_scene`-private machinery, and the next apply
        // rebuilds the root without a membership anyway. Joins and moves are synced.
        let Some(generator_membership) = world
            .get::<regia_scene::layers::LayerMembership>(generator)
            .cloned()
        else {
            continue;
        };
        let root_membership = world.get::<regia_scene::layers::LayerMembership>(root).cloned();
        if root_membership.as_ref() != Some(&generator_membership) {
            out_of_step.push((root, generator, generator_membership));
        }
    }
    for (root, generator, membership) in out_of_step {
        // A root that joins a layer late (its generator's membership appeared after the apply)
        // needs the derived guid with it: the layer switch walks members with a guid. Same
        // derivation as the apply-time copy (`output_root_guid`), a whole output's or a tile
        // cell's.
        let guid = world.get::<EntityGuid>(root).is_none().then(|| {
            let generator_guid = world.get::<EntityGuid>(generator).map_or(Guid::NIL, |g| g.0);
            let cell = world.get::<crate::tiles::ProcTileCell>(root).map(|cell| cell.key);
            EntityGuid(output_root_guid(generator_guid, cell, generator))
        });
        {
            let Ok(mut e) = world.get_entity_mut(root) else {
                continue;
            };
            match guid {
                Some(guid) => {
                    e.insert((membership, guid));
                }
                None => {
                    e.insert(membership);
                }
            }
        }
        regia_scene::layers::apply_member_presence(world, root);
    }
}

/// The static collider a generator built for its instance sets (spec 62 § 7.3): a child of its
/// output root, removed with the output. One per collision kind and data layer: the sets with a
/// `target_layer` get their own, which carries [`ProcLayerFollow`].
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcInstanceCollider {
    /// The generator.
    pub generator: Entity,
    /// `Simple`: a compound of one box per instance (the mesh's bounds); `Mesh`: one triangle mesh
    /// of every instance's mesh.
    pub kind: InstanceCollision,
    /// Instances in it.
    pub instances: usize,
}

/// The local bounds of a mesh's vertices.
fn mesh_aabb(mesh: &regia_render::CpuMesh) -> Option<(Vec3, Vec3)> {
    let mut it = mesh.vertices.iter().map(|v| Vec3::from(v.position));
    let first = it.next()?;
    Some(it.fold((first, first), |(lo, hi), p| (lo.min(p), hi.max(p))))
}

/// The colliders of an output's instance sets with `collision: Simple` or `Mesh`: a compound of
/// boxes (each instance's mesh bounds through its transform; a unit box when the host's
/// [`crate::draw::ProcMeshLibrary`] lacks the mesh) and one world-space triangle mesh of the `Mesh`
/// instances whose geometry the library has (others fall back to boxes). Physics does not take
/// triangle meshes inside compounds, so the two stay separate shapes.
/// Split per data layer (`target_layer`): sets without one first, then each layer in name order.
fn instance_colliders(world: &World, output: &RunOutput) -> Vec<LayerCollider> {
    let layers: BTreeSet<Option<&str>> = output
        .instance_sets
        .iter()
        .filter(|s| s.collision != crate::exec::Collision::None)
        .map(|s| s.layer.as_deref())
        .collect();
    layers
        .into_iter()
        .flat_map(|layer| {
            instance_colliders_in(world, output, layer)
                .into_iter()
                .map(move |(kind, n, shape)| (layer.map(str::to_string), kind, n, shape))
        })
        .collect()
}

/// One instance collider: its data layer, kind, instance count and shape.
type LayerCollider = (
    Option<String>,
    InstanceCollision,
    usize,
    regia_physics::ColliderShape,
);

/// The colliders of the sets in one data layer (`None`: the sets without a layer).
fn instance_colliders_in(
    world: &World,
    output: &RunOutput,
    layer: Option<&str>,
) -> Vec<(InstanceCollision, usize, regia_physics::ColliderShape)> {
    use crate::exec::Collision;
    use regia_physics::ColliderShape;
    let sets: Vec<&crate::exec::InstanceSet> = output
        .instance_sets
        .iter()
        .filter(|s| s.collision != Collision::None && !s.instances.is_empty() && s.layer.as_deref() == layer)
        .collect();
    if sets.is_empty() {
        return Vec::new();
    }
    let lib = world.get_resource::<crate::draw::ProcMeshLibrary>();
    let mut boxes: Vec<(Transform, ColliderShape)> = Vec::new();
    let (mut vertices, mut indices): (Vec<[f32; 3]>, Vec<u32>) = (Vec::new(), Vec::new());
    let mut mesh_instances = 0usize;
    let mut missing: BTreeSet<&str> = BTreeSet::new();
    for set in sets {
        let mesh = lib.and_then(|l| l.0.get(&set.mesh)).map(|(m, _)| m.clone());
        if mesh.is_none() {
            missing.insert(set.mesh.as_str());
        }
        let aabb = mesh.as_deref().and_then(mesh_aabb);
        let triangles = mesh.as_deref().filter(|m| {
            set.collision == Collision::Mesh
                && !m.indices.is_empty()
                && m.indices.len().is_multiple_of(3)
                && m.indices.iter().all(|&i| (i as usize) < m.vertices.len())
        });
        for inst in &set.instances {
            let raw = Quat::from_array(inst.rotation);
            let rot = if raw.is_finite() && raw.length_squared() > 1e-12 {
                raw.normalize()
            } else {
                Quat::IDENTITY
            };
            let (t, scale) = (Vec3::from(inst.translation), Vec3::from(inst.scale));
            if !t.is_finite() || !scale.is_finite() {
                continue;
            }
            let fits = triangles.is_some_and(|m| vertices.len() + m.vertices.len() <= u32::MAX as usize);
            if let (Some(m), true) = (triangles, fits) {
                let base = vertices.len() as u32;
                vertices.extend(
                    m.vertices
                        .iter()
                        .map(|v| (t + rot * (Vec3::from(v.position) * scale)).to_array()),
                );
                indices.extend(m.indices.iter().map(|i| base + i));
                mesh_instances += 1;
                continue;
            }
            let (lo, hi) = aabb.unwrap_or((Vec3::splat(-0.5), Vec3::splat(0.5)));
            let half = ((hi - lo) * 0.5 * scale.abs()).max(Vec3::splat(1e-3));
            let center = (lo + hi) * 0.5 * scale;
            boxes.push((
                Transform {
                    translation: t + rot * center,
                    rotation: rot,
                    scale: Vec3::ONE,
                },
                ColliderShape::Box { half_extents: half },
            ));
        }
    }
    if !missing.is_empty() {
        tracing::warn!(target: "regia_procgen", code = "procgen_collision_mesh_missing", "instance collision for meshes the mesh library does not have uses unit boxes: {missing:?}");
    }
    let mut out = Vec::new();
    if !boxes.is_empty() {
        let n = boxes.len();
        out.push((
            InstanceCollision::Simple,
            n,
            ColliderShape::Compound { shapes: boxes },
        ));
    }
    if let Some(shape) = ColliderShape::trimesh_from_triangles(&vertices, &indices) {
        out.push((InstanceCollision::Mesh, mesh_instances, shape));
    }
    out
}

/// Adds the terrain layers the graph's `in.ground` nodes name to a capture, weighted as a biome rule
/// weighs them: procedural by slope and altitude, blended with painted weights (spec 63 § 3).
fn capture_ground_layers(
    world: &mut World,
    compiled: &CompiledGraph,
    ground: Arc<GroundData>,
    terrain: Option<Entity>,
) -> Arc<GroundData> {
    let names: BTreeSet<String> = compiled
        .nodes
        .iter()
        .filter(|n| n.kind.id() == "in.ground")
        .flat_map(|n| n.settings.names("layers"))
        .collect();
    if names.is_empty() {
        return ground;
    }
    // One terrain's layer rules and that same terrain's paint, never one of each, and the terrain the
    // heights came from (the lowest guid), not the first one a query meets (corr-procgen).
    let (layers, painted_layers) = terrain
        .and_then(|e| {
            let t = world.get::<regia_terrain::Terrain>(e)?;
            Some((t, world.get::<regia_terrain::TerrainPaint>(e)))
        })
        .map(|(t, paint)| {
            let painted = paint
                .filter(|p| !p.0.is_blank())
                .map(|p| crate::biome::PaintedLayers::new(Arc::new(p.0.clone()), t.map.world_width()));
            (Some(Arc::new(t.layers.clone())), painted)
        })
        .unwrap_or((None, None));
    let weigher = crate::biome::BiomeWorld {
        terrain_layers: layers,
        painted_layers,
        ..crate::biome::BiomeWorld::default()
    };
    let mut ground = (*ground).clone();
    ground.capture_layers(names, |name, slope, altitude, xz| {
        weigher.layer_weight_at(name, slope, altitude, Some(xz))
    });
    Arc::new(ground)
}

/// The evaluator of a generator run on the schedule of [`ProcGenSettings::threads`].
fn run_evaluator(cache: Arc<Mutex<ProcCache>>, threads: Option<usize>) -> Evaluator {
    let evaluator = Evaluator::with_cache(cache);
    match threads {
        Some(n) => evaluator.with_parallelism(crate::exec::Parallelism::threads(n)),
        None => evaluator,
    }
}

/// Polls an in-flight run: inline jobs evaluate now; threaded ones deliver when done.
fn poll(world: &mut World, entity: Entity, ordered: bool) {
    // A run defers to a lower-guid generator that is still in flight: the first applier of an
    // overlapping terrain edit wins, and a threaded job may finish a frame before a lower-guid
    // one whose edit overlaps it. The lifecycle polls in guid order; this closes the race a
    // thread completion order would leave (audit: procgen determinism). The deferral can never
    // cycle: the lowest guid in flight never defers. Tool runs (`ordered: false`) apply
    // immediately: they are serial, and a half-applied run would fail the tool.
    if ordered && waits_behind_lower_guid(world, entity) {
        return;
    }
    let Some(job) = world
        .get_mut::<ProcGenState>(entity)
        .and_then(|mut s| s.job.take())
    else {
        return;
    };
    let result = match &job.pending {
        JobState::Inline(boxed) => {
            let cache = world.resource::<ProcGenCache>().0.clone();
            let threads = world.resource::<ProcGenSettings>().threads;
            let (compiled, inputs, tile) = &**boxed;
            Some(evaluate(
                &run_evaluator(cache, threads),
                compiled,
                inputs,
                tile.as_ref(),
                &job.cancel,
            ))
        }
        JobState::Thread(rx) => match rx.lock().try_recv() {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(RunError::Node {
                node: 0,
                code: "procgen_run_failed".into(),
                message: "the evaluation task ended without a result".into(),
            })),
        },
    };
    let run_id = job.run_id;
    let hashes = (job.graph_hash, job.param_hash);
    match result {
        None => {
            if let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
                st.job = Some(job);
            }
        }
        Some(Ok((r, tiled))) => {
            if let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
                st.evaluations += 1;
                st.state = ProcGenRunState::Applying;
            }
            if let Some(t) = &tiled {
                crate::tiles::record_baked_cells(world, entity, t);
            }
            apply(world, entity, run_id, r, hashes);
            // Only a published run is the base of the next partial run.
            if let Some(mut st) = world.get_mut::<ProcGenState>(entity)
                && st.state == ProcGenRunState::Ready
            {
                st.tiled = tiled.map(Arc::new);
            }
        }
        Some(Err(RunError::Cancelled)) => {
            // `cancel` already sent the message when it tripped the token; a token tripped by a
            // node (or a dropped job) is reported here.
            let still = world
                .get::<ProcGenState>(entity)
                .is_some_and(|s| s.state == ProcGenRunState::Running);
            if still {
                if let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
                    st.state = ProcGenRunState::Cancelled;
                }
                world.write_message(ProcGenCancelled { entity, run_id });
                log(world, entity, "cancelled", run_id);
            }
        }
        Some(Err(RunError::Node { node, code, message })) => {
            if let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
                st.evaluations += 1;
            }
            fail(world, entity, run_id, Some(node), &code, message);
        }
    }
}

/// Runs a generator to completion now (agent `procgen.run { wait: true }`, CLI bake): capture,
/// evaluate on this thread, apply. Returns the final state.
pub fn run_now(world: &mut World, entity: Entity) -> Option<ProcGenRunState> {
    run_now_with(world, entity, RunAsk::default())
}

fn run_now_with(world: &mut World, entity: Entity, ask: RunAsk) -> Option<ProcGenRunState> {
    world.get::<ProcGenerator>(entity)?;
    cancel(world, entity);
    world.resource_mut::<ProcGenQueue>().0.retain(|e| *e != entity);
    if let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
        st.refresh_run = false;
    }
    let inline = world.resource::<ProcGenSettings>().inline;
    world.resource_mut::<ProcGenSettings>().inline = true;
    start_with(world, entity, ask);
    poll(world, entity, false);
    world.resource_mut::<ProcGenSettings>().inline = inline;
    world.get::<ProcGenState>(entity).map(|s| s.state)
}

fn generator_hash(hashes: &BTreeMap<Guid, Option<[u8; 32]>>, guid: Guid) -> Option<[u8; 32]> {
    hashes.get(&guid).copied().flatten()
}

/// Finds a generator by guid text or entity name.
pub fn find_generator(world: &mut World, key: &str) -> Option<Entity> {
    let guid = key.parse::<Guid>().ok();
    let mut hits: Vec<(bool, Option<Guid>, Entity)> = world
        .query_filtered::<(Entity, Option<&EntityGuid>, Option<&Name>), With<ProcGenerator>>()
        .iter(world)
        .filter_map(|(e, g, n)| {
            let by_guid = guid.is_some_and(|x| g.is_some_and(|g| g.0 == x));
            (by_guid || n.is_some_and(|n| n.as_str() == key)).then_some((!by_guid, g.map(|g| g.0), e))
        })
        .collect();
    // Query order follows the archetypes, so a shared name must not pick by it: the guid decides.
    hits.sort_by_key(|&(by_name, guid, e)| (by_name, guid.is_none(), guid, e));
    if hits.iter().filter(|h| h.0).count() > 1 {
        tracing::warn!(target: "regia_procgen", "several generators are named `{key}`: reading the lowest guid");
    }
    hits.first().map(|h| h.2)
}

/// The generators a generator's graph reads (`in.generator_output` keys, literal settings).
///
/// The names are cached on the generator until the next file-content check. A read that fails
/// keeps the previous names, so a missing file does not look like "this graph reads nobody"
/// and release a waiter (spec 62 § 7.2).
fn generator_refs(world: &mut World, entity: Entity) -> Vec<String> {
    let generation = intern_mut(world).file_check_generation;
    if let Some(st) = world.get::<ProcGenState>(entity)
        && let Some(cached) = &st.graph_refs
        && cached.generation == generation
        && world
            .get::<ProcGenerator>(entity)
            .is_some_and(|g| g.graph == cached.graph && g.params == cached.params)
    {
        return cached.refs.clone();
    }
    match resolve_generator_refs(world, entity) {
        Some(refs) => {
            let (graph, params) = world
                .get::<ProcGenerator>(entity)
                .map(|g| (g.graph.clone(), g.params.clone()))
                .unwrap_or_default();
            if let Some(mut st) = world.get_mut::<ProcGenState>(entity) {
                st.graph_refs = Some(GraphRefs {
                    generation,
                    graph,
                    params,
                    refs: refs.clone(),
                });
            }
            refs
        }
        None => world
            .get::<ProcGenState>(entity)
            .and_then(|s| s.graph_refs.as_ref().map(|c| c.refs.clone()))
            .unwrap_or_default(),
    }
}

/// `Some` names when the graph text parses. `None` when the generator or its graph text is missing.
fn resolve_generator_refs(world: &World, entity: Entity) -> Option<Vec<String>> {
    let path = world.get::<ProcGenerator>(entity).map(|g| g.graph.clone())?;
    let resolved = resolve_graph(&path, &|p| read_graph_text(world, p)).ok()?;
    let mut refs: Vec<String> = Vec::new();
    // The generator's own parameter values bind its graph's settings; subgraphs fall back to their
    // declared defaults. A `Bind`-bound `generator` used to be missed, so ordering and the cycle
    // check ignored it (audit M12).
    let params = world
        .get::<ProcGenerator>(entity)
        .and_then(|g| g.parsed_params().ok())
        .unwrap_or_default();
    let top = resolved.merged_with(&params);
    let subgraphs =
        crate::graph::collect_subgraphs_with(&resolved.graph, &top, &|p| read_graph_text(world, p));
    let mut graphs = vec![(resolved.graph.clone(), top)];
    graphs.extend(
        subgraphs
            .values()
            .filter_map(|t| crate::graph::ProcGraph::from_ron(t).ok())
            .map(|g| (g, BTreeMap::new())),
    );
    for (g, values) in &graphs {
        for n in g.nodes.values().filter(|n| n.ty == "in.generator_output") {
            let named = match n.settings.get("generator") {
                Some(SettingValue::Str(k)) => Some(k.clone()),
                Some(v) => v.as_bind().and_then(|b| {
                    values
                        .get(b)
                        .or_else(|| g.params.iter().find(|p| p.name == b).map(|p| &p.default))
                        .and_then(|v| match v {
                            SettingValue::Str(k) => Some(k.clone()),
                            _ => None,
                        })
                }),
                None => None,
            };
            refs.extend(named);
        }
    }
    refs.sort();
    refs.dedup();
    Some(refs)
}

/// Whether `reader` must stay queued (spec 62 § 7.2).
///
/// An idle or cleared source that this host can run is queued first, so it publishes before the
/// reader starts. A source that is already ready, or that has failed or been cancelled, is not
/// started again. A tiled source, or an `editor_only` source outside the editor, is not started
/// from here: the read warns `export_missing` when that source published nothing.
fn reader_blocked_on_sources(world: &mut World, reader: Entity) -> bool {
    let mut blocked = false;
    for key in generator_refs(world, reader) {
        let Some(source) = find_generator(world, &key) else {
            continue;
        };
        if source == reader {
            continue;
        }
        let state = world
            .get::<ProcGenState>(source)
            .map(|s| s.state)
            .unwrap_or(ProcGenRunState::Idle);
        if state.in_flight() {
            blocked = true;
            continue;
        }
        if !matches!(state, ProcGenRunState::Idle | ProcGenRunState::Cleared) {
            continue;
        }
        let skip = editor_only_refused(world, source)
            || world
                .get::<ProcGenerator>(source)
                .is_some_and(crate::tiles::is_scheduled);
        if skip {
            continue;
        }
        request_run(world, source);
        blocked = true;
    }
    blocked
}

/// Whether following generator reads from `start` comes back to it (`generator_cycle`).
fn on_generator_cycle(world: &mut World, start: Entity) -> bool {
    let mut stack = vec![start];
    let mut seen = BTreeSet::new();
    while let Some(e) = stack.pop() {
        for key in generator_refs(world, e) {
            let Some(d) = find_generator(world, &key) else {
                continue;
            };
            if d == start {
                return true;
            }
            if seen.insert(d) {
                stack.push(d);
            }
        }
    }
    false
}

/// Marks generators stale whose dependencies changed (spec 62 § 5.3).
fn detect_stale(world: &mut World) {
    crate::terrain_edits::take_author_edit(world);
    let mut edits: Vec<([f32; 4], Option<Guid>)> = world
        .get_resource_mut::<Messages<ProcTerrainEdited>>()
        .map(|mut m| m.drain().map(|e| (e.rect, e.source)).collect())
        .unwrap_or_default();
    // Repainted layer weights are a terrain change over the whole map (spec 65 § 7).
    let repainted = world
        .query_filtered::<&regia_terrain::Terrain, Changed<regia_terrain::TerrainPaint>>()
        .iter(world)
        .next()
        .map(|t| t.map.world_width() * 0.5);
    if let Some(h) = repainted {
        edits.push(([-h, -h, h, h], None));
    }
    let moved: BTreeSet<Guid> = world
        .query_filtered::<&EntityGuid, (
            Without<Generated>,
            Or<(
                Changed<Transform>,
                Changed<Tags>,
                Changed<regia_scene::spline::Spline>,
                Changed<regia_scene::shape::Shape>,
                Changed<crate::biome::ecs::BiomeArea>,
            )>,
        )>()
        .iter(world)
        .map(|g| g.0)
        .collect();
    // Walk tags only when a finished generator has a query dependency, and reuse the previous
    // list when no tag changed and nothing was spawned or despawned. Hashes come only from
    // generators, which is the same answer as scanning every guid (a missing one is `None`).
    let entity_len = u32::try_from(u64::from(world.entities().len())).unwrap_or(u32::MAX);
    // A removed `Tags` (or a despawned tagged entity) neither changes `Tags` nor, with a spawn in the
    // same frame, the entity count: the removal buffer covers both (corr-procgen). A re-run also
    // despawns the generator's own `Generated` outputs, which carry `Tags`: a despawn removes every
    // component, so an entity in the `Generated` removal buffer that no longer exists was one of
    // those outputs, not a tag edit (fixrev2-procgen-k). An entity that only lost `Generated`
    // (a kept output) is still alive and its `Tags` removals still count.
    let generated_despawned: BTreeSet<Entity> = world
        .removed::<Generated>()
        .filter(|e| world.get_entity(*e).is_err())
        .collect();
    let tags_changed = world
        .query_filtered::<Entity, (Without<Generated>, Changed<Tags>)>()
        .iter(world)
        .next()
        .is_some()
        || world.removed::<Tags>().any(|e| !generated_despawned.contains(&e));
    let needs_tags = world.query::<&ProcGenState>().iter(world).any(|s| {
        !s.state.in_flight()
            && s.run_id != 0
            && s.deps.iter().any(|d| matches!(d, ProcDependency::Query { .. }))
    });
    let needs_hashes = world.query::<&ProcGenState>().iter(world).any(|s| {
        !s.state.in_flight()
            && s.run_id != 0
            && s.deps
                .iter()
                .any(|d| matches!(d, ProcDependency::Generator { .. }))
    });
    let mut restore_tags = false;
    let tagged_now: Vec<(Guid, Vec<String>)> = if !needs_tags {
        if world.contains_resource::<ProcGenIntern>() {
            world.resource_mut::<ProcGenIntern>().tags_valid = false;
        }
        Vec::new()
    } else {
        let reuse = world
            .get_resource::<ProcGenIntern>()
            .is_some_and(|intern| intern.tags_valid && intern.entity_len == entity_len && !tags_changed);
        if reuse {
            restore_tags = true;
            std::mem::take(&mut world.resource_mut::<ProcGenIntern>().tag_snapshot)
        } else {
            let built: Vec<(Guid, Vec<String>)> = world
                .query_filtered::<(&EntityGuid, &Tags), Without<Generated>>()
                .iter(world)
                .filter(|(_, t)| !t.0.is_empty())
                .map(|(g, t)| {
                    crate::probe::hit_idle_scan();
                    (g.0, t.0.iter().map(|n| n.as_str().to_string()).collect())
                })
                .collect();
            let mut intern = intern_mut(world);
            intern.tag_snapshot.clone_from(&built);
            intern.tags_valid = true;
            intern.entity_len = entity_len;
            built
        }
    };
    let world_hashes: BTreeMap<Guid, Option<[u8; 32]>> = if needs_hashes {
        world
            .query::<(&EntityGuid, &ProcGenState)>()
            .iter(world)
            .map(|(g, s)| {
                crate::probe::hit_idle_scan();
                (g.0, s.output_hash)
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    let world_hashes = &world_hashes;
    // Files by content hash (spec 62 § 5.3), every `file_check_frames` frames: each file once.
    let check_files = {
        let every = world.resource::<ProcGenSettings>().file_check_frames;
        let mut watch = world.resource_mut::<ProcGenWatch>();
        watch.frames_since_file_check = watch.frames_since_file_check.saturating_add(1);
        let due = watch.frames_since_file_check > every;
        if due {
            watch.frames_since_file_check = 0;
        }
        due
    };
    if check_files {
        let mut intern = intern_mut(world);
        intern.file_check_generation = intern.file_check_generation.wrapping_add(1);
    }
    let file_now: BTreeMap<String, Option<[u8; 32]>> = if check_files {
        let _span = tracing::info_span!("procgen.file_check").entered();
        // At runtime only generators that refresh pay for the reads (the editor checks all).
        let editor = world.resource::<ProcGenSettings>().editor;
        let paths: BTreeSet<String> = world
            .query::<(&ProcGenerator, &ProcGenState)>()
            .iter(world)
            .filter(|(g, s)| (editor || g.refresh_on_change) && !s.state.in_flight() && s.run_id != 0)
            .flat_map(|(_, s)| s.file_hashes.keys().cloned())
            .collect();
        paths
            .into_iter()
            .map(|p| {
                let h = file_content_hash(world, &p);
                (p, h)
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    let gens: Vec<Entity> = world
        .query_filtered::<Entity, With<ProcGenerator>>()
        .iter(world)
        .collect();
    for e in gens {
        let own = world.get::<EntityGuid>(e).map(|g| g.0);
        // Bounds taken from the generator's own Shape or Spline move with them.
        let own_bounds = world
            .get::<ProcGenerator>(e)
            .is_some_and(|g| !matches!(g.bounds, ProcBounds::Box { .. }))
            && own.is_some_and(|g| moved.contains(&g))
            && world.get::<ProcGenState>(e).and_then(|s| s.run_bounds) != Some(generator_bounds(world, e));
        let Some(mut st) = world.get_mut::<ProcGenState>(e) else {
            continue;
        };
        if st.state.in_flight() || st.run_id == 0 {
            continue;
        }
        let terrain_rects: Vec<[f32; 4]> = st
            .deps
            .iter()
            .filter_map(|d| match d {
                ProcDependency::Terrain { rect } => Some(*rect),
                _ => None,
            })
            .flat_map(|rect| {
                edits.iter().filter_map(move |(r, source)| {
                    let foreign = source.is_none_or(|s| s != own.unwrap_or(Guid::NIL));
                    (foreign && r[0] <= rect[2] && rect[0] <= r[2] && r[1] <= rect[3] && rect[1] <= r[3])
                        .then_some(*r)
                })
            })
            .collect();
        let other = st.deps.iter().any(|d| match d {
            // Handled above: a generator's own edit is not a change to its inputs (spec 64 § 7.3).
            ProcDependency::Terrain { .. } => false,
            ProcDependency::Entity { guid } => moved.contains(guid) && Some(*guid) != own,
            ProcDependency::Query {
                tags,
                any_tags,
                matched,
            } => {
                let now: BTreeSet<Guid> = tagged_now
                    .iter()
                    .filter(|(g, t)| Some(*g) != own && tags_match(t, tags, any_tags))
                    .map(|(g, _)| *g)
                    .collect();
                now != *matched
            }
            ProcDependency::File { path } => file_now
                .get(path)
                .is_some_and(|now| st.file_hashes.get(path).is_some_and(|then| then != now)),
            ProcDependency::Generator { guid, output_hash } => {
                generator_hash(world_hashes, *guid) != *output_hash
            }
        }) || own_bounds;
        if !terrain_rects.is_empty() {
            match st.tiled.clone() {
                // A terrain change has a rectangle: only the cells around it are stale (spec 66 § 2).
                Some(run) => {
                    for r in &terrain_rects {
                        let change = r.map(f64::from);
                        let keys = crate::tiles::stale_cells(&run.plan, &run.bounds, run.halo_m, change);
                        st.stale_cells.extend(keys);
                    }
                }
                None => st.stale_whole = true,
            }
        }
        if other {
            st.stale_whole = true;
        }
        if other || !terrain_rects.is_empty() {
            st.stale = true;
        }
    }
    if restore_tags {
        world.resource_mut::<ProcGenIntern>().tag_snapshot = tagged_now;
    }
}

/// The lifecycle system: staleness, refresh requests, starting queued runs within the
/// concurrency limit, and polling in-flight runs.
pub fn procgen_update(world: &mut World) {
    let _span = tracing::info_span!("procgen.update").entered();
    watch_bake_dir(world);
    load_pending_bakes(world);
    crate::terrain_edits::apply_waiting(world);
    detect_stale(world);
    gate_admit(world);
    let settings = world.resource::<ProcGenSettings>().clone();

    // Stale generators that refresh: editor `live` (after the debounce) or runtime `refresh_on_change`.
    let gens: Vec<(Entity, Guid)> = world
        .query::<(Entity, &ProcGenerator, Option<&EntityGuid>)>()
        .iter(world)
        .map(|(e, _, g)| (e, g.map_or(Guid::NIL, |g| g.0)))
        .collect();
    let mut refresh = Vec::new();
    for (e, guid) in &gens {
        let wants = world.get::<ProcGenerator>(*e).is_some_and(|g| {
            if settings.editor {
                g.live
            } else {
                g.refresh_on_change
            }
        });
        if let Some(mut st) = world.get_mut::<ProcGenState>(*e)
            && st.stale
            && wants
            && !st.state.in_flight()
        {
            st.stale_frames += 1;
            if !settings.editor || st.stale_frames >= settings.debounce_frames {
                refresh.push((*guid, *e));
            }
        }
    }
    refresh.sort();
    for (_, e) in refresh {
        request_run(world, e);
        if let Some(mut st) = world.get_mut::<ProcGenState>(e) {
            st.refresh_run = true;
        }
    }

    // Poll in-flight runs first so their slots free up, in guid order: the first applier of an
    // overlapping terrain edit wins, so the poll order decides ownership (audit: procgen
    // determinism).
    let running: Vec<Entity> = world
        .query::<(Entity, &ProcGenState)>()
        .iter(world)
        .filter(|(_, s)| s.job.is_some())
        .map(|(e, _)| e)
        .collect();
    let mut running: Vec<((bool, Guid), Entity)> =
        running.into_iter().map(|e| (guid_key(world, e), e)).collect();
    running.sort();
    for (_, e) in running {
        poll(world, e, true);
    }

    // Start queued runs up to the limit, in guid order (guid-less last): the first applier of
    // an overlapping terrain edit wins, so the start order must not depend on request order —
    // with one slot (the Android default) request order used to let a higher-guid run apply
    // before a lower-guid one still waiting for the slot, flipping the winner per platform
    // (ADR-5447). Between runs of the same guid key the request order is kept (stable sort).
    let in_flight = world
        .query::<&ProcGenState>()
        .iter(world)
        .filter(|s| s.job.is_some())
        .count();
    let free = settings.max_concurrent_runs.saturating_sub(in_flight);
    // A generator that reads another generator's output waits until that one has reached `ready`
    // (spec 62 § 7.2). An idle or cleared source is queued ahead of the reader. A read cycle fails
    // with `generator_cycle`.
    let mut queue: Vec<Entity> = std::mem::take(&mut world.resource_mut::<ProcGenQueue>().0);
    queue.sort_by_cached_key(|&e| guid_key(world, e));
    let mut waiting = Vec::new();
    let mut started = 0usize;
    for e in queue {
        if world.get::<ProcGenerator>(e).is_none() {
            continue;
        }
        if started >= free {
            waiting.push(e);
            continue;
        }
        if on_generator_cycle(world, e) {
            let run_id = world.get::<ProcGenState>(e).map_or(0, |s| s.run_id) + 1;
            if let Some(mut st) = state_mut(world, e) {
                st.run_id = run_id;
            }
            fail(
                world,
                e,
                run_id,
                None,
                "generator_cycle",
                "generators read each other's output".to_string(),
            );
            continue;
        }
        if reader_blocked_on_sources(world, e) {
            waiting.push(e);
            continue;
        }
        // `None` is a scheduled generator or a missing state: cells were re-queued, or there is
        // nothing to start. Either way it must not take a concurrency slot.
        if start(world, e).is_some() {
            started += 1;
        }
    }
    world.resource_mut::<ProcGenQueue>().0.extend(waiting);
    gate_check(world);
}

/// Notes the level of a `LevelLoaded` for the load report (spec 62 § 7.4).
pub fn procgen_level_loaded(
    mut loaded: MessageReader<regia_scene::level::LevelLoaded>,
    mut gate: ResMut<ProcGenLoadGate>,
) {
    if let Some(l) = loaded.read().last() {
        gate.level = Some(l.level);
    }
}

/// A new bake directory (a level change, spec 62 § 8): generators that never ran here look for their
/// bake again, in the new directory, and anchored baked-streaming generators that found no file (or
/// no directory) in the old one open the new one's (their `procgen_bake_missing` latch clears). Runs
/// at the start of the tile schedule and of the lifecycle update, whichever sees the change first.
pub(crate) fn watch_bake_dir(world: &mut World) {
    let Some(dir) = world
        .get_resource::<ProcGenSettings>()
        .map(|s| s.bake_dir.clone())
    else {
        return;
    };
    if world
        .get_resource::<ProcGenWatch>()
        .is_none_or(|w| w.bake_dir == dir)
    {
        return;
    }
    world.resource_mut::<ProcGenWatch>().bake_dir = dir;
    let mut q = world.query::<&mut ProcGenState>();
    for mut st in q.iter_mut(world) {
        if st.run_id == 0 && !st.state.in_flight() {
            st.bake_checked = false;
        }
    }
    let cleared: Vec<Entity> = world
        .get_resource::<crate::tiles::ProcGenTiles>()
        .map(|t| {
            t.errors
                .iter()
                .filter(|(_, (code, _))| code == "procgen_bake_missing" || code == "procgen_no_bake_dir")
                .map(|(e, _)| *e)
                .collect()
        })
        .unwrap_or_default();
    for e in cleared {
        if let Some(mut tiles) = world.get_resource_mut::<crate::tiles::ProcGenTiles>() {
            tiles.errors.remove(&e);
        }
        if let Some(mut host) = world.get_mut::<crate::tiles::ProcGenTileHost>(e) {
            host.error = None;
        }
    }
}

/// Sets the bake directory to the one of the level at `level` (`<level>.procgen/`, spec 62 § 8), or
/// none. A host calls it when it opens another level; the generators that never ran load their bakes
/// from there on the next update.
pub fn follow_level(world: &mut World, level: Option<&std::path::Path>) {
    let dir = level.map(level_bake_dir);
    if let Some(mut s) = world.get_resource_mut::<ProcGenSettings>()
        && s.bake_dir != dir
    {
        s.bake_dir = dir;
    }
}

/// What the load gate waits for in a generator: `on_load` (its first run) or `anchored` (the cells
/// around the start anchor, `wait_on_load`).
fn gate_kind(g: &ProcGenerator) -> Option<&'static str> {
    if g.mode == ProcGenMode::OnLoad && !crate::tiles::is_scheduled(g) {
        Some("on_load")
    } else if crate::tiles::is_scheduled(g)
        && g.tiles
            .as_ref()
            .and_then(|t| t.runtime.as_ref())
            .is_some_and(|r| r.wait_on_load)
    {
        Some("anchored")
    } else {
        None
    }
}

/// Requests the first run of `OnLoad` generators that spawned (runtime only; the editor runs every
/// mode on demand) and opens the load gate for them and for anchored generators with
/// `wait_on_load` (spec 62 § 7.4, spec 66 § 4.3).
fn gate_admit(world: &mut World) {
    if world.resource::<ProcGenSettings>().editor {
        return;
    }
    // Admit in guid order: the queue starts runs in order, and the first applier of an
    // overlapping terrain edit wins (audit: procgen determinism).
    let mut new: Vec<((bool, Guid), Entity, &'static str)> = world
        .query::<(Entity, &ProcGenerator, Option<&ProcGenState>, Option<&EntityGuid>)>()
        .iter(world)
        .filter(|(_, _, s, _)| s.is_none_or(|s| !s.on_load_requested && s.run_id == 0))
        .filter_map(|(e, g, _, id)| {
            gate_kind(g).map(|k| ((id.is_none(), id.map_or(Guid::NIL, |g| g.0)), e, k))
        })
        .collect();
    new.sort();
    for (_, e, kind) in new {
        if kind == "on_load" {
            request_run(world, e);
        }
        if let Some(mut st) = state_mut(world, e) {
            st.on_load_requested = true;
        }
        // Its run fails with `procgen_editor_only`: nothing to wait for.
        if editor_only_refused(world, e) {
            continue;
        }
        let mut gate = world.resource_mut::<ProcGenLoadGate>();
        if gate.open.is_none() {
            let level = gate.level.take();
            gate.open = Some(ProcGenLoadReport {
                level,
                ..ProcGenLoadReport::default()
            });
            gate.started = Some(std::time::Instant::now());
        }
        gate.waiting.insert(e, kind);
    }
    let mut gate = world.resource_mut::<ProcGenLoadGate>();
    if gate.open.is_none() {
        gate.level = None;
    }
}

/// Closes the load gate when every generator it waits for is done, or fails it visibly at
/// [`ProcGenSettings::on_load_timeout_frames`] (`procgen_on_load_timeout`; the late runs still
/// publish when they finish). Sends [`ProcGenLoadFinished`].
fn gate_check(world: &mut World) {
    if world.resource::<ProcGenLoadGate>().open.is_none() {
        return;
    }
    let timeout = world.resource::<ProcGenSettings>().on_load_timeout_frames;
    let waiting: Vec<(Entity, &'static str)> = world
        .resource::<ProcGenLoadGate>()
        .waiting
        .iter()
        .map(|(e, k)| (*e, *k))
        .collect();
    let mut done: Vec<(Entity, &'static str, ProcGenRunState, Option<String>)> = Vec::new();
    for (e, kind) in &waiting {
        let st = world.get::<ProcGenState>(*e);
        let finished = if world.get::<ProcGenerator>(*e).is_none() {
            Some((
                ProcGenRunState::Cleared,
                Some("procgen_not_generator".to_string()),
            ))
        } else if *kind == "on_load" {
            st.filter(|s| s.run_id > 0 && !s.state.in_flight()).map(|s| {
                let code = (s.state != ProcGenRunState::Ready).then(|| {
                    s.last_error
                        .as_ref()
                        .map_or_else(|| s.state.name().to_string(), |x| x.1.clone())
                });
                (s.state, code)
            })
        } else {
            let tiles = world.resource::<crate::tiles::ProcGenTiles>();
            if let Some((code, _)) = tiles.errors.get(e) {
                Some((ProcGenRunState::Failed, Some(code.clone())))
            } else {
                tiles
                    .generators
                    .get(e)
                    .filter(|g| {
                        g.entries.values().all(|c| {
                            matches!(
                                c.state,
                                crate::tiles::TileState::Resident
                                    | crate::tiles::TileState::Failed
                                    | crate::tiles::TileState::Releasing
                            )
                        })
                    })
                    .map(|g| {
                        let failed = g
                            .entries
                            .values()
                            .any(|c| c.state == crate::tiles::TileState::Failed);
                        if failed {
                            (ProcGenRunState::Failed, Some("procgen_cell_failed".to_string()))
                        } else {
                            (ProcGenRunState::Ready, None)
                        }
                    })
            }
        };
        if let Some((state, code)) = finished {
            done.push((*e, *kind, state, code));
        }
    }
    let guids: BTreeMap<Entity, Guid> = waiting
        .iter()
        .map(|(e, _)| (*e, world.get::<EntityGuid>(*e).map_or(Guid::NIL, |g| g.0)))
        .collect();
    let states: BTreeMap<Entity, ProcGenRunState> = waiting
        .iter()
        .map(|(e, _)| {
            (
                *e,
                world
                    .get::<ProcGenState>(*e)
                    .map_or(ProcGenRunState::Idle, |s| s.state),
            )
        })
        .collect();
    let mut gate = world.resource_mut::<ProcGenLoadGate>();
    let ms = gate.started.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
    let Some(mut report) = gate.open.take() else {
        return;
    };
    report.frames = report.frames.saturating_add(1);
    let guid = |e: &Entity| guids.get(e).copied().unwrap_or(Guid::NIL);
    for (e, mode, state, code) in done {
        gate.waiting.remove(&e);
        report.entries.push(ProcGenLoadEntry {
            entity: e,
            guid: guid(&e),
            mode,
            state,
            frames: report.frames,
            ms,
            code,
        });
    }
    if !gate.waiting.is_empty() && report.frames >= timeout {
        let late: Vec<(Entity, &'static str)> = std::mem::take(&mut gate.waiting).into_iter().collect();
        for (e, mode) in late {
            tracing::error!(target: "regia_procgen", code = "procgen_on_load_timeout", generator = %guid(&e), frames = report.frames, "the level load timed out waiting for a generator; it publishes when it finishes");
            report.entries.push(ProcGenLoadEntry {
                entity: e,
                guid: guid(&e),
                mode,
                state: states.get(&e).copied().unwrap_or_default(),
                frames: report.frames,
                ms,
                code: Some("procgen_on_load_timeout".to_string()),
            });
        }
        report.timed_out = true;
    }
    if !gate.waiting.is_empty() {
        gate.open = Some(report);
        return;
    }
    report.ms = ms;
    report.entries.sort_by_key(|x| (x.guid, x.entity));
    gate.started = None;
    gate.last = Some(report.clone());
    world.write_message(ProcGenLoadFinished { report });
}

/// The bake directory of a level: `<level path without extension>.procgen` (spec 62 § 8). The
/// file and directory forms of a level (spec 37) share one bake.
pub fn level_bake_dir(level: &std::path::Path) -> PathBuf {
    let name = level.file_name().map(|n| n.to_string_lossy().into_owned());
    let stem = name
        .as_deref()
        .and_then(|n| {
            n.strip_suffix(".level.ron")
                .or_else(|| n.strip_suffix(".level.d"))
        })
        .map(str::to_string)
        .or_else(|| level.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    level.with_file_name(format!("{stem}.procgen"))
}

/// The `.pgout` path of a generator in a bake directory.
pub fn bake_path(dir: &std::path::Path, guid: Guid) -> PathBuf {
    dir.join(format!("{guid}.pgout"))
}

/// The `.pgout` path of one tier of a `PerTier` generator: `<guid>.<Tier>.pgout` (`High`,
/// `MobileLow`, ...; spec 62 § 7.4).
pub fn bake_tier_path(dir: &std::path::Path, guid: Guid, tier: QualityTier) -> PathBuf {
    dir.join(format!("{guid}.{}.pgout", tier.name()))
}

/// Every bake file of a generator that exists in `dir`: the plain file and the per-tier files.
pub fn bake_files(dir: &std::path::Path, guid: Guid) -> Vec<PathBuf> {
    std::iter::once(bake_path(dir, guid))
        .chain(QualityTier::ALL.iter().map(|t| bake_tier_path(dir, guid, *t)))
        .filter(|p| p.is_file())
        .collect()
}

/// The guid a bake file belongs to (`<guid>.pgout` or `<guid>.<Tier>.pgout`), if the name is one.
pub fn bake_file_guid(name: &str) -> Option<Guid> {
    let stem = name.strip_suffix(".pgout")?;
    let guid = match stem.split_once('.') {
        Some((g, tier)) => {
            QualityTier::from_name(tier)?;
            g
        }
        None => stem,
    };
    guid.parse().ok()
}

/// The tiers to try for a running tier, best first: itself, the lower tiers nearest first, then the
/// higher ones nearest first (spec 62 § 7.4).
pub fn tier_fallback(running: QualityTier) -> Vec<QualityTier> {
    // Very Low has no bakes of its own: it loads Low's (ADR-7882).
    let running = running.content_tier();
    let i = crate::nodes::quality_index(running);
    let mut out = vec![running];
    out.extend((0..i).rev().map(|k| QualityTier::ALL[k]));
    out.extend((i + 1..QualityTier::ALL.len()).map(|k| QualityTier::ALL[k]));
    out
}

/// The bake file a generator loads and its tier (`PerTier`: the running tier's file or the nearest
/// fallback that exists; `None` tier for a plain bake).
/// The asset path (`assets/levels/a.procgen/<guid>.pgout`) of a bake file, as the cook records it
/// in the pak manifest: `path` relative to the project `root` (or already relative), `/`
/// separators, under `assets/`. `None` for a path outside the project or one with `..`.
pub fn bake_asset_path(root: &std::path::Path, path: &std::path::Path) -> Option<String> {
    use std::path::Component;
    let rel = path.strip_prefix(root).unwrap_or(path);
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let joined = parts.join("/");
    if joined.starts_with("assets/") {
        Some(joined)
    } else {
        Some(format!("assets/{joined}"))
    }
}

/// Whether the asset server's paks hold the bake file `path` (a packaged game ships bakes only
/// there, ADR-7214).
fn bake_in_paks(world: &World, path: &std::path::Path) -> bool {
    world
        .get_resource::<regia_asset::AssetServer>()
        .and_then(|s| bake_asset_path(s.project_root(), path).and_then(|p| s.id_for_path(&p)))
        .is_some()
}

/// The bytes of a bake file: the file on disk when it exists, else its cooked copy in the paks the
/// asset server mounted (spec 62 § 8, ADR-7214: `regia package` ships bakes in the pak, where an
/// Android game can read them). `None` when neither has it.
pub fn read_bake_bytes(world: &mut World, path: &std::path::Path) -> Option<Vec<u8>> {
    read_bake(world, path).map(|(bytes, _)| bytes)
}

/// [`read_bake_bytes`] with where the bytes came from.
pub(crate) fn read_bake(world: &mut World, path: &std::path::Path) -> Option<(Vec<u8>, ProcBakeSource)> {
    if let Ok(bytes) = std::fs::read(path) {
        return Some((bytes, ProcBakeSource::Disk));
    }
    let mut server = world.get_resource_mut::<regia_asset::AssetServer>()?;
    let rel = bake_asset_path(server.project_root(), path)?;
    let id = server.id_for_path(&rel)?;
    let (file, _) = server.read_cooked(id).ok()?;
    let (header, payload) = regia_asset::decode_cooked_file(&file).ok()?;
    (header.type_tag == regia_core::id::TypePathHash::of(crate::pgout::PGOUT_COOKED_TYPE_PATH))
        .then(|| (payload.to_vec(), ProcBakeSource::Pak))
}

/// Bake directories of the levels a game loaded, by level id (spec 62 § 8, ADR-7262): each is
/// `level_bake_dir` of the level's source path. The loader falls back to it for a generator that a
/// placed level (inline or streamed) or another additive level brought in, whose bake
/// `regia procgen bake` wrote beside that level under the generator's source guid. The `regia`
/// crate's default plugins fill it from the levels `load_level_source` read; a host that loads
/// levels another way inserts the entries itself. A level missing here is not guessed.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcLevelBakeDirs(pub BTreeMap<regia_scene::level::LevelId, PathBuf>);

/// Where a loaded bake's bytes came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcBakeSource {
    /// A `.pgout` file on disk (a dev run, or a desktop package's loose file).
    Disk,
    /// The cooked copy in a mounted pak (a packaged game, ADR-7214).
    Pak,
}

impl ProcBakeSource {
    /// The agent name (`disk`, `pak`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Pak => "pak",
        }
    }
}

/// The bake a `Baked` generator's published output was loaded from (ADR-7262), or the cell bake an
/// anchored (`Baked` streaming) generator streams its cells from (ADR-7287); removed when a run
/// replaces the output, the output is cleared, or a streamed generator's file is reopened or
/// missing.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct ProcBakeOrigin {
    /// The bake file (`<level>.procgen/<guid>.pgout` or a per-tier file).
    pub path: PathBuf,
    /// The level whose bake directory held it, when it was not the primary
    /// [`ProcGenSettings::bake_dir`] (a placed or additive level's, from [`ProcLevelBakeDirs`]).
    pub level: Option<regia_scene::level::LevelId>,
    /// Disk or pak.
    pub source: ProcBakeSource,
}

/// The level and source guid a generator came from when a placed or additive level spawned it
/// (ADR-7262): an inline placement's member carries `derive_guid(placement, source)`, so the
/// source is found among the placed level's records, including the `added_children` of its prefab
/// instances at any depth (ADR-7287; the placement derives their guids the same way); a streamed
/// placement or an additive level keeps the source guid and tags the member with its level.
fn placed_origin(world: &World, entity: Entity, guid: Guid) -> Option<(regia_scene::level::LevelId, Guid)> {
    use regia_scene::placed::{PlacedMember, PlacedMode, PlacedSpawned};
    if let Some(member) = world.get::<PlacedMember>(entity)
        && let Some(spawned) = world.get::<PlacedSpawned>(member.instance)
        && spawned.mode == PlacedMode::Inline
    {
        let placement = world.get::<EntityGuid>(member.instance)?.0;
        let level = world
            .get_resource::<regia_scene::level::LevelLibrary>()?
            .get(spawned.level)?;
        let records = &level.scene.entities;
        let source = member_source(world, entity, guid, &|g| inline_source(records, placement, g))?;
        return Some((spawned.level, source));
    }
    let level = world.get::<regia_scene::level::LevelMember>(entity)?.level();
    Some((level, guid))
}

/// Most ancestors [`member_source`] walks to find a prefab-internal entity's instance root.
const MAX_PREFAB_WALK: usize = 4096;

/// The source guid of an inline-placed member `entity` (guid `guid`): `top(guid)` for a record of
/// the placed level or an added child (see [`inline_source`]); else, for a prefab-internal entity
/// (ADR-7331: `PrefabLink`, guid `derived_guid(root, local_id)`), the instance root is the nearest
/// ancestor whose guid derives `guid` with the entity's local id, and the source is
/// `derived_guid(source of that root, local_id)`: the guid the same entity has in the source level.
/// Nested prefabs recurse through their own roots. `None` when nothing maps back.
fn member_source(
    world: &World,
    entity: Entity,
    guid: Guid,
    top: &dyn Fn(Guid) -> Option<Guid>,
) -> Option<Guid> {
    use regia_scene::prefab::{PrefabLink, derived_guid};
    if let Some(source) = top(guid) {
        return Some(source);
    }
    let local = world.get::<PrefabLink>(entity)?.local_id;
    let mut cursor = world.get::<ChildOf>(entity).map(ChildOf::parent);
    for _ in 0..MAX_PREFAB_WALK {
        let ancestor = cursor?;
        if let Some(root) = world.get::<EntityGuid>(ancestor).map(|g| g.0)
            && derived_guid(root, local) == guid
        {
            let root_source = member_source(world, ancestor, root, top)?;
            return Some(derived_guid(root_source, local));
        }
        cursor = world.get::<ChildOf>(ancestor).map(ChildOf::parent);
    }
    None
}

/// The record guid among `records` (and, depth first, the `added_children` of their prefab
/// instances) whose inline-placed guid `derive_guid(placement, record)` is `guid` (ADR-7287).
/// Top-level records are tried before any added child.
fn inline_source(records: &[regia_scene::format::SceneEntity], placement: Guid, guid: Guid) -> Option<Guid> {
    use regia_scene::placed::derive_guid;
    records
        .iter()
        .map(|r| r.guid)
        .find(|g| derive_guid(placement, *g) == guid)
        .or_else(|| {
            records.iter().find_map(|r| {
                r.prefab_instance
                    .as_ref()
                    .and_then(|i| inline_source(&i.added_children, placement, guid))
            })
        })
}

/// Whether a bake file exists on disk or in the mounted paks.
fn bake_exists(world: &World, path: &std::path::Path) -> bool {
    path.is_file() || bake_in_paks(world, path)
}

/// The bake a generator loads: its file in the primary directory `dir`, else (a placed or
/// additive level's generator, ADR-7262) its file in that level's directory under its source
/// guid. The third value names that level; with neither present, the primary path is answered.
fn locate_bake(
    world: &World,
    entity: Entity,
    dir: &std::path::Path,
) -> (PathBuf, Option<QualityTier>, Option<regia_scene::level::LevelId>) {
    locate_bake_by(world, entity, dir, &|dir, guid| {
        bake_file_for_guid(world, entity, dir, guid)
    })
}

/// The cell bake an anchored (`Baked` streaming) generator opens (ADR-7287): `<guid>.pgout` in the
/// primary directory `dir`, else, for a placed or additive level's generator, the file under its
/// source guid in that level's directory from [`ProcLevelBakeDirs`]. The host level's own file
/// wins where both exist. The second value names the level whose directory held it; with
/// neither present, the primary path is answered.
pub(crate) fn locate_cell_bake(
    world: &World,
    entity: Entity,
    dir: &std::path::Path,
) -> (PathBuf, Option<regia_scene::level::LevelId>) {
    let (path, _, level) = locate_bake_by(world, entity, dir, &|dir, guid| (bake_path(dir, guid), None));
    (path, level)
}

/// Names a generator's bake file (and its tier) in a directory under a guid.
type BakeFileFor<'a> = &'a dyn Fn(&std::path::Path, Guid) -> (PathBuf, Option<QualityTier>);

/// [`locate_bake`] with `file_for(dir, guid)` naming a generator's file in a directory.
fn locate_bake_by(
    world: &World,
    entity: Entity,
    dir: &std::path::Path,
    file_for: BakeFileFor<'_>,
) -> (PathBuf, Option<QualityTier>, Option<regia_scene::level::LevelId>) {
    let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    let (path, tier) = file_for(dir, guid);
    if bake_exists(world, &path) {
        return (path, tier, None);
    }
    if let Some((level, source)) = placed_origin(world, entity, guid)
        && let Some(level_dir) = world
            .get_resource::<ProcLevelBakeDirs>()
            .and_then(|d| d.0.get(&level))
        && level_dir.as_path() != dir
    {
        let (other, other_tier) = file_for(level_dir, source);
        if bake_exists(world, &other) {
            return (other, other_tier, Some(level));
        }
    }
    (path, tier, None)
}

/// Where a generator's bake is written and verified (ADR-7331): the directory and the guid that
/// name its file, and the level whose directory that is (`None` = the primary directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakeTarget {
    /// The bake directory.
    pub dir: PathBuf,
    /// The guid the file is named after (`<guid>.pgout`, `<guid>.<Tier>.pgout`).
    pub guid: Guid,
    /// The level whose directory `dir` is, from [`ProcLevelBakeDirs`]; `None` for the primary
    /// [`ProcGenSettings::bake_dir`].
    pub level: Option<regia_scene::level::LevelId>,
}

/// The bake a generator's re-bake writes and `verify` compares against (ADR-7331, the ADR-7287 (3)
/// rule): its own guid in the primary directory `dir` when a file of it exists there (on disk or in
/// a pak); else, for a member a placed or additive level brought in whose level is in
/// [`ProcLevelBakeDirs`], that level's directory under the member's source guid (where
/// `regia procgen bake <that level>` writes it); else the primary directory under its own guid.
pub fn bake_target(world: &World, entity: Entity, dir: &std::path::Path) -> BakeTarget {
    let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    let primary = BakeTarget {
        dir: dir.to_path_buf(),
        guid,
        level: None,
    };
    if has_bake_in(world, dir, guid) {
        return primary;
    }
    let Some((level, source)) = placed_origin(world, entity, guid) else {
        return primary;
    };
    match world
        .get_resource::<ProcLevelBakeDirs>()
        .and_then(|d| d.0.get(&level))
    {
        Some(level_dir) if level_dir.as_path() != dir => BakeTarget {
            dir: level_dir.clone(),
            guid: source,
            level: Some(level),
        },
        _ => primary,
    }
}

/// Whether `dir` holds any bake file of `guid` (the plain file or a tier's), on disk or in a pak.
fn has_bake_in(world: &World, dir: &std::path::Path, guid: Guid) -> bool {
    std::iter::once(bake_path(dir, guid))
        .chain(QualityTier::ALL.iter().map(|t| bake_tier_path(dir, guid, *t)))
        .any(|p| bake_exists(world, &p))
}

/// The guid an unpinned seed derives from (ADR-7331): the generator's own, except for a `Baked`
/// inline-placed member whose bake is its source level's (see [`bake_target`]), which runs with the
/// source guid's seed, so its re-bake writes the bytes the source level's own bake writes and the
/// loaded file is not stale for its seed alone.
fn seed_guid(world: &World, entity: Entity) -> Option<Guid> {
    let own = world.get::<EntityGuid>(entity).map(|g| g.0)?;
    let baked = world
        .get::<ProcGenerator>(entity)
        .is_some_and(|g| g.mode == ProcGenMode::Baked);
    if !baked || world.get::<regia_scene::placed::PlacedMember>(entity).is_none() {
        return Some(own);
    }
    match world
        .get_resource::<ProcGenSettings>()
        .and_then(|s| s.bake_dir.as_deref())
    {
        Some(dir) => Some(bake_target(world, entity, dir).guid),
        None => Some(own),
    }
}

/// The bake file a generator loads from `dir` under `guid` (its own, or a placed generator's source
/// guid) and its tier: a `PerTier` generator's running tier or the nearest fallback that exists,
/// else the plain file.
fn bake_file_for_guid(
    world: &World,
    entity: Entity,
    dir: &std::path::Path,
    guid: Guid,
) -> (PathBuf, Option<QualityTier>) {
    let per_tier = world
        .get::<ProcGenerator>(entity)
        .is_some_and(|g| g.quality_tier == Some(ProcBakeTier::PerTier));
    if !per_tier {
        return (bake_path(dir, guid), None);
    }
    let running = world.resource::<ProcGenSettings>().running_tier;
    tier_fallback(running)
        .into_iter()
        .map(|t| (bake_tier_path(dir, guid, t), Some(t)))
        .find(|(p, _)| bake_exists(world, p))
        .unwrap_or_else(|| (bake_tier_path(dir, guid, running), Some(running)))
}

fn quality_index(world: &World, entity: Entity, tier: Option<QualityTier>) -> u8 {
    let g = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    let tier = tier.unwrap_or_else(|| run_quality(&g, world.resource::<ProcGenSettings>()));
    crate::nodes::quality_index(tier) as u8
}

/// blake3 over a run's dependencies, in order, with the content hashes of its files (the `.pgout`
/// input hash; spec 62 § 8).
pub fn input_hash_with_files(
    deps: &[ProcDependency],
    files: &BTreeMap<String, Option<[u8; 32]>>,
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&input_hash(deps));
    for (path, hash) in files {
        h.update(path.as_bytes());
        h.update(&hash.unwrap_or([0; 32]));
    }
    *h.finalize().as_bytes()
}

/// blake3 over a run's dependencies. The hash is independent of the capture order: the
/// dependencies are sorted by a total key first, so a save/load cycle (which changes the ECS
/// query order the capture walked) cannot change the baked header (see [`input_hash_with_files`]).
pub fn input_hash(deps: &[ProcDependency]) -> [u8; 32] {
    let mut sorted: Vec<&ProcDependency> = deps.iter().collect();
    sorted.sort_by_key(|d| dep_sort_bytes(d));
    let mut h = blake3::Hasher::new();
    for d in sorted {
        match d {
            ProcDependency::Terrain { rect } => {
                h.update(b"terrain");
                rect.iter().for_each(|v| {
                    h.update(&v.to_bits().to_le_bytes());
                });
            }
            ProcDependency::Entity { guid } => {
                h.update(b"entity");
                h.update(&guid.0.to_le_bytes());
            }
            ProcDependency::Query {
                tags,
                any_tags,
                matched,
            } => {
                h.update(b"query");
                // Framed: `["ab"]`, `["a"] + any ["b"]` and `["a", "b"]` hash apart.
                for list in [tags, any_tags] {
                    h.update(&(list.len() as u64).to_le_bytes());
                    for t in list {
                        h.update(&(t.len() as u64).to_le_bytes());
                        h.update(t.as_bytes());
                    }
                }
                matched.iter().for_each(|g| {
                    h.update(&g.0.to_le_bytes());
                });
            }
            ProcDependency::File { path } => {
                h.update(b"file");
                h.update(path.as_bytes());
            }
            ProcDependency::Generator { guid, output_hash } => {
                h.update(b"generator");
                h.update(&guid.0.to_le_bytes());
                h.update(&output_hash.unwrap_or([0; 32]));
            }
        }
    }
    *h.finalize().as_bytes()
}

/// A total order over [`ProcDependency`] for [`input_hash`]: variant, then payload.
fn dep_sort_bytes(d: &ProcDependency) -> Vec<u8> {
    let mut v = Vec::new();
    match d {
        ProcDependency::Terrain { rect } => {
            v.push(0);
            for x in rect {
                v.extend_from_slice(&x.to_bits().to_le_bytes());
            }
        }
        ProcDependency::Entity { guid } => {
            v.push(1);
            v.extend_from_slice(&guid.0.to_le_bytes());
        }
        ProcDependency::Query {
            tags,
            any_tags,
            matched,
        } => {
            v.push(2);
            // The same framing as the hash, so distinct deps never tie in the sort.
            for list in [tags, any_tags] {
                v.extend_from_slice(&(list.len() as u64).to_le_bytes());
                for t in list {
                    v.extend_from_slice(&(t.len() as u64).to_le_bytes());
                    v.extend_from_slice(t.as_bytes());
                }
            }
            for g in matched {
                v.extend_from_slice(&g.0.to_le_bytes());
            }
        }
        ProcDependency::File { path } => {
            v.push(3);
            v.extend_from_slice(path.as_bytes());
        }
        ProcDependency::Generator { guid, output_hash } => {
            v.push(4);
            v.extend_from_slice(&guid.0.to_le_bytes());
            v.extend_from_slice(&output_hash.unwrap_or([0; 32]));
        }
    }
    v
}

/// A bake, verify or load failure: a stable code and a message.
pub type BakeError = (String, String);

/// `procgen_not_baked` unless `entity` is a generator in `Baked` mode.
fn require_baked(world: &World, entity: Entity) -> Result<(), BakeError> {
    match world.get::<ProcGenerator>(entity).map(|g| g.mode) {
        Some(ProcGenMode::Baked) => Ok(()),
        Some(mode) => Err((
            "procgen_not_baked".to_string(),
            format!(
                "the generator's mode is {}; only Baked generators bake",
                mode.name()
            ),
        )),
        None => Err(("procgen_not_generator".to_string(), "not a generator".to_string())),
    }
}

/// Runs a `Baked` generator now and writes its `.pgout` atomically (spec 62 § 8). Needs the
/// `pipeline` feature to write.
///
/// # Errors
///
/// The run's failure code (`check_failed`, `procgen_graph_invalid`, ...), `procgen_no_bake_dir`, or
/// the file error (`pgout_io`, `procgen_editor_only`); `procgen_not_baked` for a generator whose
/// mode is not `Baked` (its `.pgout` would load as current if the mode were flipped later; audit
/// M18).
pub fn bake(world: &mut World, entity: Entity) -> Result<PathBuf, BakeError> {
    require_baked(world, entity)?;
    let dir = world
        .resource::<ProcGenSettings>()
        .bake_dir
        .clone()
        .ok_or_else(|| {
            (
                "procgen_no_bake_dir".to_string(),
                "no bake directory is set".to_string(),
            )
        })?;
    let generator = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    let per_tier = generator.quality_tier == Some(ProcBakeTier::PerTier);
    if crate::tiles::is_baked_streaming(&generator) {
        if per_tier {
            return Err((
                "procgen_per_tier_streaming".to_string(),
                "a generator that streams its baked cells bakes one tier; set quality_tier to a tier"
                    .to_string(),
            ));
        }
        let target = bake_target(world, entity, &dir);
        return bake_streamed(world, entity, &target);
    }
    // A placed or additive member's bake goes to its source level's directory under its source
    // guid unless the primary directory has its own file (ADR-7331).
    let BakeTarget { dir, guid, .. } = bake_target(world, entity, &dir);
    if !per_tier {
        let path = bake_path(&dir, guid);
        bake_one(world, entity, &path, None)?;
        // A plain bake replaces the files of an earlier `PerTier` setting.
        for t in QualityTier::ALL {
            let _ = std::fs::remove_file(bake_tier_path(&dir, guid, t));
        }
        return Ok(path);
    }
    let tiers = {
        let t = &world.resource::<ProcGenSettings>().bake_tiers;
        if t.is_empty() {
            QualityTier::ALL.to_vec()
        } else {
            t.clone()
        }
    };
    for tier in &tiers {
        bake_one(world, entity, &bake_tier_path(&dir, guid, *tier), Some(*tier))?;
    }
    // Tiers no longer in the list, and a plain file of an earlier setting, would load instead.
    for t in QualityTier::ALL.iter().filter(|t| !tiers.contains(t)) {
        let _ = std::fs::remove_file(bake_tier_path(&dir, guid, *t));
    }
    let _ = std::fs::remove_file(bake_path(&dir, guid));
    // Publish what the running tier loads.
    let (path, _) = bake_file_for_guid(world, entity, &dir, guid);
    load_bake(world, entity)?;
    Ok(path)
}

/// Runs a generator now as a tool (at `tier` when given) and writes its output to `path`.
fn bake_one(
    world: &mut World,
    entity: Entity,
    path: &std::path::Path,
    tier: Option<QualityTier>,
) -> Result<(), BakeError> {
    let state = run_now_with(world, entity, RunAsk { tool: true, tier })
        .ok_or_else(|| ("procgen_not_generator".to_string(), "not a generator".to_string()))?;
    let st = world
        .get::<ProcGenState>(entity)
        .ok_or_else(|| ("procgen_not_generator".to_string(), String::new()))?;
    if state != ProcGenRunState::Ready {
        let (code, msg) = st
            .last_error
            .as_ref()
            .map_or(("procgen_run_failed".to_string(), String::new()), |e| {
                (e.1.clone(), e.2.clone())
            });
        return Err((code, msg));
    }
    let (graph_hash, param_hash) = st.hashes.unwrap_or(([0; 32], [0; 32]));
    let header = crate::pgout::PgoutHeader {
        graph_hash,
        param_hash,
        seed: generator_seed(world, entity),
        input_hash: input_hash_with_files(&st.deps, &st.file_hashes),
        output_hash: st.output_hash.unwrap_or([0; 32]),
        quality: quality_index(world, entity, tier),
    };
    let output = world
        .get::<ProcOutput>(entity)
        .map(|o| o.0.clone())
        .ok_or_else(|| ("procgen_run_failed".to_string(), "no output".to_string()))?;
    let tiled = world.get::<ProcGenState>(entity).and_then(|s| s.tiled.clone());
    // `spawn.prefab` overrides go into the file's override section (ADR-7750).
    let bytes = match &tiled {
        Some(t) => encode_tiled(&header, t),
        None => crate::pgout::encode(&header, &output),
    };
    crate::pgout::write_atomic(path, &bytes).map_err(|e| (e.code.to_string(), e.message))?;
    if let Some(mut st) = state_mut(world, entity) {
        st.baked_tier = tier;
    }
    Ok(())
}

/// A tiled bake: one section per cell (spec 66 § 2).
fn encode_tiled(header: &crate::pgout::PgoutHeader, run: &crate::tiles::TiledRun) -> Vec<u8> {
    let cells: Vec<crate::pgout::CellInput<'_>> = run
        .cells
        .iter()
        .map(|(k, c)| (k.tier, k.cell.x as i32, k.cell.z as i32, c.output.as_ref()))
        .collect();
    crate::pgout::encode_cells(header, &cells)
}

/// The inputs of a bake or verify run (the `Baked` tier, the generator's limits and edit gate).
fn bake_inputs(
    world: &World,
    entity: Entity,
    snap: StaticSnapshot,
    files: &[String],
    tier: Option<QualityTier>,
) -> RunInputs {
    let generator = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    let settings = world.resource::<ProcGenSettings>();
    let mut inputs = RunInputs::new(Arc::new(snap), generator_seed(world, entity))
        .with_graphs(subgraph_texts(world, files));
    if let Some(l) = generator.limits {
        inputs.limits = inputs.limits.with_override(&l.to_override());
    }
    inputs.quality = tier.unwrap_or_else(|| run_quality(&generator, settings));
    inputs.terrain_edits = terrain_edit_gate(&generator, settings);
    inputs
}

/// Bakes a streaming generator without publishing: its cells stay on disk until anchors load them
/// (spec 66 § 4.3 "Baked + anchors"); resident cells reload from the new file.
fn bake_streamed(world: &mut World, entity: Entity, target: &BakeTarget) -> Result<PathBuf, BakeError> {
    let (compiled, files) = compile_for(world, entity).map_err(|(c, m, _)| (c, m))?;
    let (snap, mut deps) = capture(world, entity, &compiled);
    deps.extend(files.iter().cloned().map(|path| ProcDependency::File { path }));
    let file_hashes = file_hashes_of(world, &deps);
    let inputs = bake_inputs(world, entity, snap, &files, None);
    let generator = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    let tile = tile_ctx(&generator, &compiled, generator_bounds(world, entity))?;
    let (result, tiled) = evaluate(
        &Evaluator::new(),
        &compiled,
        &inputs,
        tile.as_ref(),
        &CancelToken::new(),
    )
    .map_err(|e| (e.code().to_string(), e.to_string()))?;
    let Some(tiled) = tiled else {
        return Err((
            "procgen_not_tiled".to_string(),
            "the generator has no tiles".to_string(),
        ));
    };
    let header = crate::pgout::PgoutHeader {
        graph_hash: compiled.graph_hash,
        param_hash: compiled.param_hash,
        seed: generator_seed(world, entity),
        input_hash: input_hash_with_files(&deps, &file_hashes),
        output_hash: result.output_hash,
        quality: quality_index(world, entity, None),
    };
    let path = bake_path(&target.dir, target.guid);
    crate::pgout::write_atomic(&path, &encode_tiled(&header, &tiled))
        .map_err(|e| (e.code.to_string(), e.message))?;
    crate::tiles::reset_host(world, entity);
    if let Some(mut st) = state_mut(world, entity) {
        st.deps = deps;
        st.file_hashes = file_hashes;
        st.stale = false;
        st.output_hash = Some(result.output_hash);
        st.hashes = Some((compiled.graph_hash, compiled.param_hash));
    }
    Ok(path)
}

/// Re-bakes the named cells of a tiled generator into its `.pgout` (spec 66 § 2): every other
/// cell's section is copied byte for byte, the file is replaced atomically, and a generator that
/// publishes its bake whole reloads it. Coarser cells a key reads are run and discarded.
///
/// # Errors
///
/// `procgen_not_tiled`, `procgen_bake_missing` when there is no file yet, the run's failure code,
/// or the [`crate::pgout::PgoutError`] code.
pub fn bake_cells(
    world: &mut World,
    entity: Entity,
    keys: &[crate::tiles::TileKey],
) -> Result<PathBuf, BakeError> {
    require_baked(world, entity)?;
    let dir = world
        .resource::<ProcGenSettings>()
        .bake_dir
        .clone()
        .ok_or_else(|| {
            (
                "procgen_no_bake_dir".to_string(),
                "no bake directory is set".to_string(),
            )
        })?;
    let generator = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    let (compiled, files) = compile_for(world, entity).map_err(|(c, m, _)| (c, m))?;
    let Some(tile) = tile_ctx(&generator, &compiled, generator_bounds(world, entity))? else {
        return Err((
            "procgen_not_tiled".to_string(),
            "the generator has no tiles".to_string(),
        ));
    };
    for key in keys {
        if tile.plan.tier(key.tier).is_none() {
            return Err((
                "procgen_tier_unknown".to_string(),
                format!("the graph has no tier {}", key.tier),
            ));
        }
        if !crate::tiles::tier_cells(&tile.plan, key.tier, &tile.bounds).contains(key) {
            return Err((
                "procgen_unknown_cell".to_string(),
                format!(
                    "cell ({}, {}) of tier {} is not in the generator bounds",
                    key.cell.x, key.cell.z, key.tier
                ),
            ));
        }
    }
    let target = bake_target(world, entity, &dir);
    let path = bake_path(&target.dir, target.guid);
    let mut bytes = std::fs::read(&path).map_err(|_| {
        (
            "procgen_bake_missing".to_string(),
            format!("{} is missing", path.display()),
        )
    })?;
    let (snap, _) = capture(world, entity, &compiled);
    let inputs = bake_inputs(world, entity, snap, &files, None);
    let eval = Evaluator::new();
    let job = crate::tiles::TileJob {
        evaluator: &eval,
        graph: &compiled,
        plan: &tile.plan,
        inputs: &inputs,
        bounds: &tile.bounds,
        halo_m: tile.halo_m,
    };
    for key in keys {
        let cell = crate::tiles::run_cell(&job, *key, &CancelToken::new())
            .map_err(|e| (e.code().to_string(), e.to_string()))?;
        bytes = crate::pgout::replace_cell(
            &bytes,
            key.tier,
            key.cell.x as i32,
            key.cell.z as i32,
            cell.output.as_ref(),
        )
        .map_err(|e| (e.code.to_string(), e.message))?;
    }
    crate::pgout::write_atomic(&path, &bytes).map_err(|e| (e.code.to_string(), e.message))?;
    if crate::tiles::is_baked_streaming(&generator) {
        crate::tiles::reset_host(world, entity);
    } else {
        crate::terrain_edits::logged(world, entity, |w| load_bake(w, entity))?;
    }
    Ok(path)
}

/// Loads a generator's `.pgout` and publishes it without running the graph.
///
/// # Errors
///
/// `procgen_bake_missing` when the file does not exist, or the [`crate::pgout::PgoutError`] code.
pub fn load_bake(world: &mut World, entity: Entity) -> Result<[u8; 32], BakeError> {
    if editor_only_refused(world, entity) {
        return Err((
            "procgen_editor_only".to_string(),
            "the generator is editor_only: its bake is not loaded outside the editor".to_string(),
        ));
    }
    let dir = world
        .resource::<ProcGenSettings>()
        .bake_dir
        .clone()
        .ok_or_else(|| {
            (
                "procgen_no_bake_dir".to_string(),
                "no bake directory is set".to_string(),
            )
        })?;
    let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    let (path, baked_tier, origin_level) = locate_bake(world, entity, &dir);
    let (bytes, source) = read_bake(world, &path).ok_or_else(|| {
        (
            "procgen_bake_missing".to_string(),
            format!("{} is missing", path.display()),
        )
    })?;
    let (header, output) = crate::pgout::decode(&bytes).map_err(|e| (e.code.to_string(), e.message))?;
    // The file is served, but marked stale when the graph, its parameters or the seed changed since
    // the bake (audit M11: an edited graph used to load as current until someone ran verify).
    let current = compile_for(world, entity)
        .ok()
        .map(|(c, _)| (c.graph_hash, c.param_hash, generator_seed(world, entity)));
    let outdated = current != Some((header.graph_hash, header.param_hash, header.seed));
    if outdated {
        tracing::warn!(target: "regia_procgen", "{}: the bake does not match the current graph, parameters or seed; re-bake", path.display());
    }
    crate::terrain_edits::publish(world, entity, &output.terrain_edits)?;
    despawn_outputs(world, entity);
    let output = Arc::new(output);
    spawn_prefabs(world, entity, guid, 0, &output);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert((
            ProcOutput(output),
            ProcBakeOrigin {
                path: path.clone(),
                level: origin_level,
                source,
            },
        ));
    }
    if let Some(mut st) = state_mut(world, entity) {
        st.state = ProcGenRunState::Ready;
        st.output_hash = Some(header.output_hash);
        st.hashes = Some((header.graph_hash, header.param_hash));
        st.bake_checked = true;
        st.stale = outdated;
        st.tiled = None;
        st.baked_tier = baked_tier;
    }
    Ok(header.output_hash)
}

/// The result of re-running a baked generator against its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyResult {
    /// The re-run's output hash equals the file's.
    pub ok: bool,
    /// The file's output hash (`None` when the file is missing or unreadable).
    pub file_hash: Option<[u8; 32]>,
    /// The re-run's output hash.
    pub run_hash: [u8; 32],
    /// Why the file could not be read, if it could not.
    pub file_error: Option<String>,
    /// A `PerTier` generator: each baked tier and whether its file matches (the fields above are the
    /// first mismatch, else the last tier). Empty for a plain bake.
    pub tiers: Vec<(QualityTier, bool)>,
}

/// Re-runs a generator (without publishing) and compares the output hash with its `.pgout`
/// (`regia procgen verify`, spec 62 § 8).
///
/// # Errors
///
/// The run's failure code, `procgen_no_bake_dir`, or `procgen_not_baked` (see [`bake`]).
pub fn verify(world: &mut World, entity: Entity) -> Result<VerifyResult, BakeError> {
    require_baked(world, entity)?;
    let dir = world
        .resource::<ProcGenSettings>()
        .bake_dir
        .clone()
        .ok_or_else(|| {
            (
                "procgen_no_bake_dir".to_string(),
                "no bake directory is set".to_string(),
            )
        })?;
    let (compiled, files) = compile_for(world, entity).map_err(|(c, m, _)| (c, m))?;
    let generator = world.get::<ProcGenerator>(entity).cloned().unwrap_or_default();
    // The file a re-bake would write: a placed member's is its source level's (ADR-7331).
    let BakeTarget { dir, guid, .. } = bake_target(world, entity, &dir);
    let per_tier = generator.quality_tier == Some(ProcBakeTier::PerTier);
    let tiers: Vec<Option<QualityTier>> = if per_tier {
        let t = &world.resource::<ProcGenSettings>().bake_tiers;
        if t.is_empty() {
            QualityTier::ALL.iter().copied().map(Some).collect()
        } else {
            t.iter().copied().map(Some).collect()
        }
    } else {
        vec![None]
    };
    let tile = tile_ctx(&generator, &compiled, generator_bounds(world, entity))?;
    let mut out: Option<VerifyResult> = None;
    let mut per = Vec::new();
    for tier in tiers {
        let (snap, _) = capture(world, entity, &compiled);
        let inputs = bake_inputs(world, entity, snap, &files, tier);
        let (r, _) = evaluate(
            &Evaluator::new(),
            &compiled,
            &inputs,
            tile.as_ref(),
            &CancelToken::new(),
        )
        .map_err(|e| (e.code().to_string(), e.to_string()))?;
        let path = tier.map_or_else(|| bake_path(&dir, guid), |t| bake_tier_path(&dir, guid, t));
        let file = std::fs::read(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|b| crate::pgout::decode(&b).map_err(|e| e.to_string()));
        let one = match file {
            Ok((h, _)) => VerifyResult {
                ok: h.output_hash == r.output_hash,
                file_hash: Some(h.output_hash),
                run_hash: r.output_hash,
                file_error: None,
                tiers: Vec::new(),
            },
            Err(e) => VerifyResult {
                ok: false,
                file_hash: None,
                run_hash: r.output_hash,
                file_error: Some(e),
                tiers: Vec::new(),
            },
        };
        if let Some(t) = tier {
            per.push((t, one.ok));
        }
        if out.as_ref().is_none_or(|o| o.ok) {
            out = Some(one);
        }
    }
    let mut result = out.ok_or_else(|| ("procgen_no_tiers".to_string(), "no tiers to verify".to_string()))?;
    result.ok = result.ok && per.iter().all(|(_, ok)| *ok);
    result.tiers = per;
    Ok(result)
}

/// An entity whose anchored generator's cell bake exists but which has no [`ProcGenerator`]
/// (ADR-7331 (3)): a level saved while ADR-7287 (5) was open (`snapshot_scene` dropped the
/// generator of every anchored generator) kept the record without it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LostAnchoredGenerator {
    /// The entity that lost its generator.
    pub entity: Entity,
    /// Its guid.
    pub guid: Guid,
    /// The level whose bake directory holds the cell bake (from [`ProcLevelBakeDirs`]), else the
    /// entity's own level (`LevelMember`), else `None`.
    pub level: Option<regia_scene::level::LevelId>,
    /// The cell bake (`<guid>.pgout`).
    pub path: PathBuf,
}

/// The guids of the plain `<guid>.pgout` files in `dir` (per-tier files are whole bakes).
fn plain_bake_guids(dir: &std::path::Path) -> BTreeSet<Guid> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let stem = name.to_str()?.strip_suffix(".pgout")?;
            stem.parse::<Guid>().ok()
        })
        .collect()
}

/// Whether the file at `path` is a cell bake: more than one section, or one that is not the
/// untiled `(tier 0, 0, 0)` section a whole bake has.
fn is_cell_bake(path: &std::path::Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| crate::pgout::read_table(&bytes).ok())
        .is_some_and(|(_, cells)| cells.len() > 1 || cells.iter().any(|c| (c.tier, c.i, c.j) != (0, 0, 0)))
}

/// Every entity with an [`EntityGuid`] but no [`ProcGenerator`] whose guid names a cell bake on disk
/// (ADR-7331 (3), `regia procgen verify` and `procgen.verify` report each as
/// `anchored_generator_missing`): in the primary [`ProcGenSettings::bake_dir`] under its own guid, or,
/// for a member a placed or additive level brought in, in that level's directory from
/// [`ProcLevelBakeDirs`] under its source guid. Read only; sorted by guid. Without the generator the
/// cell bake cannot say what it was, so the finding names it and nothing is rewritten. A level that
/// lost an anchored generator whose cells were never baked cannot be told from a plain entity.
pub fn lost_anchored_generators(world: &mut World) -> Vec<LostAnchoredGenerator> {
    let _span = tracing::info_span!(target: "regia_procgen", "lost_anchored_generators").entered();
    let primary = world
        .get_resource::<ProcGenSettings>()
        .and_then(|s| s.bake_dir.clone());
    let levels: Vec<(regia_scene::level::LevelId, PathBuf, BTreeSet<Guid>)> = world
        .get_resource::<ProcLevelBakeDirs>()
        .map(|d| {
            d.0.iter()
                .filter(|(_, dir)| Some(dir.as_path()) != primary.as_deref())
                .map(|(level, dir)| (*level, dir.clone(), plain_bake_guids(dir)))
                .filter(|(_, _, guids)| !guids.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let primary_guids = primary.as_deref().map(plain_bake_guids).unwrap_or_default();
    if primary_guids.is_empty() && levels.is_empty() {
        return Vec::new();
    }
    let candidates: Vec<(Entity, Guid)> = world
        .query_filtered::<(Entity, &EntityGuid), Without<ProcGenerator>>()
        .iter(world)
        .map(|(e, g)| (e, g.0))
        .collect();
    let world = &*world;
    let mut out = Vec::new();
    for (entity, guid) in candidates {
        let own_level = world
            .get::<regia_scene::level::LevelMember>(entity)
            .map(regia_scene::level::LevelMember::level);
        let found = if let Some(dir) = primary.as_deref().filter(|_| primary_guids.contains(&guid)) {
            Some((own_level, bake_path(dir, guid)))
        } else if levels.is_empty()
            || (own_level.is_none() && world.get::<regia_scene::placed::PlacedMember>(entity).is_none())
        {
            None
        } else {
            placed_origin(world, entity, guid).and_then(|(level, source)| {
                levels
                    .iter()
                    .find(|(l, _, guids)| *l == level && guids.contains(&source))
                    .map(|(_, dir, _)| (Some(level), bake_path(dir, source)))
            })
        };
        if let Some((level, path)) = found
            && is_cell_bake(&path)
        {
            out.push(LostAnchoredGenerator {
                entity,
                guid,
                level,
                path,
            });
        }
    }
    out.sort_by_key(|l| (l.guid, l.entity));
    out
}

/// Loads the bakes of `Baked` generators that have none published yet (once per generator).
fn load_pending_bakes(world: &mut World) {
    if world.resource::<ProcGenSettings>().bake_dir.is_none() {
        return;
    }
    let editor = world.resource::<ProcGenSettings>().editor;
    let pending: Vec<Entity> = world
        .query::<(Entity, &ProcGenerator, Option<&ProcGenState>)>()
        .iter(world)
        .filter(|(_, g, s)| {
            g.mode == ProcGenMode::Baked
                && !crate::tiles::is_baked_streaming(g)
                && (editor || !g.editor_only)
                && s.is_none_or(|s| !s.bake_checked && s.run_id == 0)
        })
        .map(|(e, _, _)| e)
        .collect();
    for e in pending {
        if let Err((code, msg)) = load_bake(world, e) {
            tracing::warn!(target: "regia_procgen", code = code.as_str(), "procgen_bake_missing: {msg}");
        }
        if let Some(mut st) = state_mut(world, e) {
            st.bake_checked = true;
        }
    }
}

/// The signature of [`ProcMeshWriter`]: `(mesh, name, path)`.
pub type ProcMeshWrite =
    dyn Fn(&crate::exec::GeneratedMesh, &str, &std::path::Path) -> Result<(), String> + Send + Sync;

/// Writes a generated mesh as a mesh asset file (filled by the editor, which has the asset
/// pipeline).
#[derive(Resource, Clone)]
pub struct ProcMeshWriter(pub Arc<ProcMeshWrite>);

/// What [`detach`] made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detached {
    /// The new root (the former output root, now ordinary content).
    pub root: Entity,
    /// Former generated entities, now saved.
    pub entities: Vec<Entity>,
    /// One `InstancedMeshSet` entity per instance set.
    pub instance_sets: Vec<Entity>,
    /// One mesh entity per generated mesh (`MeshRenderer`, and `MeshSource` when it was written).
    pub meshes: Vec<Entity>,
    /// Generated meshes that could not be written: they draw this session and are not saved.
    pub unsaved_meshes: usize,
}

/// Turns the output's generated meshes into mesh entities under `root`: each is written under
/// `<bake dir>/detached/` when a [`ProcMeshWriter`] and a bake directory exist.
/// `path` as a level stores it: relative to the project with forward slashes, so the level works on
/// another machine. A path outside the project stays as it is.
fn project_path(source: &ProcGraphSource, path: &std::path::Path) -> String {
    let relative = match source {
        ProcGraphSource::Dir(root) => path.strip_prefix(root).ok(),
        _ => None,
    };
    relative.map_or_else(
        || path.display().to_string(),
        |r| {
            r.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        },
    )
}

fn detach_meshes(world: &mut World, root: Entity, guid: Guid, output: &RunOutput) -> (Vec<Entity>, usize) {
    let writer = world.get_resource::<ProcMeshWriter>().cloned();
    let settings = world.resource::<ProcGenSettings>();
    let (dir, source) = (settings.bake_dir.clone(), settings.source.clone());
    let materials = world.get_resource::<crate::draw::ProcMaterialLibrary>().cloned();
    let mut entities = Vec::with_capacity(output.meshes.len());
    let mut unsaved = 0;
    for (k, m) in output.meshes.iter().enumerate() {
        let name = format!("generated mesh {k}");
        let saved = match (&writer, &dir) {
            (Some(w), Some(d)) => {
                let path = d.join("detached").join(format!("{guid}_{k}.glb"));
                match (w.0)(m, &name, &path) {
                    Ok(()) => Some(path),
                    Err(e) => {
                        tracing::warn!(target: "regia_procgen", code = "procgen_detach_mesh_unsaved", "{}: {e}", path.display());
                        None
                    }
                }
            }
            _ => None,
        };
        let material = materials
            .as_ref()
            .and_then(|l| l.0.get(&m.material).cloned())
            .unwrap_or_default();
        let renderer = regia_render::MeshRenderer::new(Arc::new(crate::draw::cpu_mesh(m)), material);
        let mut e = world.spawn((
            Transform::default(),
            GlobalTransform::default(),
            EntityGuid::new_random(),
            Name::new(name.as_str()),
            renderer,
            ChildOf(root),
        ));
        match saved {
            Some(path) => {
                e.insert(regia_render::MeshSource::new(
                    project_path(&source, &path),
                    m.material.as_str(),
                ));
            }
            None => unsaved += 1,
        }
        entities.push(e.id());
    }
    (entities, unsaved)
}

/// Converts a generator's current output into ordinary level content (spec 62 § 7.3): spawned
/// entities lose `Generated` and get guids so the level saves them, instance sets become
/// `InstancedMeshSet` entities under the root, generated meshes become mesh entities written as
/// assets under `<bake dir>/detached/` ([`ProcMeshWriter`]), terrain edits stay applied, the
/// generator's bake file is deleted and its output cleared. Sends `ProcGenDetached`.
///
/// # Errors
///
/// `procgen_not_generator`, or `procgen_no_output` when nothing is published.
pub fn detach(world: &mut World, entity: Entity) -> Result<Detached, BakeError> {
    if world.get::<ProcGenerator>(entity).is_none() {
        return Err(("procgen_not_generator".into(), "not a generator".into()));
    }
    cancel(world, entity);
    let output = world
        .get::<ProcOutput>(entity)
        .map(|o| o.0.clone())
        .ok_or_else(|| {
            (
                "procgen_no_output".to_string(),
                "the generator has no published output".to_string(),
            )
        })?;
    let root = world
        .query::<(Entity, &ProcOutputRoot)>()
        .iter(world)
        .find(|(_, r)| r.generator == entity)
        .map(|(e, _)| e)
        .unwrap_or_else(|| {
            world
                .spawn((Transform::default(), GlobalTransform::default()))
                .id()
        });
    let generated: Vec<Entity> = world
        .query_filtered::<(Entity, &ChildOf), With<Generated>>()
        .iter(world)
        .filter(|(_, c)| c.parent() == root)
        .map(|(e, _)| e)
        .collect();
    for e in generated.iter().copied().chain([root]) {
        if let Ok(mut m) = world.get_entity_mut(e) {
            m.remove::<Generated>();
            m.remove::<ProcOutputRoot>();
            // A root in a layer has a guid derived from the generator: the next run's root would
            // get the same one, so the detached root gets its own.
            if e == root || !m.contains::<EntityGuid>() {
                m.insert(EntityGuid::new_random());
            }
        }
    }
    let mut sets = Vec::new();
    for set in &output.instance_sets {
        let e = world
            .spawn((
                InstancedMeshSet::from_set(set),
                Transform::default(),
                GlobalTransform::default(),
                EntityGuid::new_random(),
                Name::new(format!("instances {}", set.mesh).as_str()),
                ChildOf(root),
            ))
            .id();
        sets.push(e);
    }
    let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
    let (meshes, unsaved_meshes) = detach_meshes(world, root, guid, &output);
    // Terrain edits stay applied: they are ordinary terrain content now (spec 62 § 7.3).
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.remove::<ProcOutput>();
        e.remove::<crate::terrain_edits::ProcTerrainApplied>();
    }
    // The bake is the only copy of a mesh that could not be saved: it stays until a detach saves all.
    if let (Some(dir), 0) = (
        world.resource::<ProcGenSettings>().bake_dir.clone(),
        unsaved_meshes,
    ) {
        let guid = world.get::<EntityGuid>(entity).map_or(Guid::NIL, |g| g.0);
        // A missing bake is fine: there is nothing to delete.
        for path in bake_files(&dir, guid) {
            let _ = std::fs::remove_file(path);
        }
    }
    if let Some(mut st) = state_mut(world, entity) {
        st.state = ProcGenRunState::Cleared;
        st.output_hash = None;
        st.hashes = None;
        st.tiled = None;
    }
    world.write_message(ProcGenDetached {
        entity,
        new_root: root,
    });
    log(world, entity, "detached", 0);
    Ok(Detached {
        root,
        entities: generated,
        instance_sets: sets,
        meshes,
        unsaved_meshes,
    })
}

/// Runs `graph` for `entity` (its parameters, seed, bounds and captured world) at once and keeps
/// every node's output, without publishing anything: the editor's "preview up to here" (spec 67
/// § 3). The generator's state, output and spawned entities are untouched.
///
/// # Errors
///
/// `(node, code, message)`: `procgen_not_generator`, the compile code with the first error's node,
/// or the failing node's code.
pub fn evaluate_detached(
    world: &mut World,
    entity: Entity,
    graph: &crate::ProcGraph,
) -> Result<crate::RunResult, (Option<u32>, String, String)> {
    let _span = tracing::info_span!("procgen.evaluate_detached").entered();
    let Some(generator) = world.get::<ProcGenerator>(entity).cloned() else {
        return Err((None, "procgen_not_generator".into(), "not a generator".into()));
    };
    let registry = world.resource::<ProcGenRegistry>().0.clone();
    let params = generator.parsed_params().map_err(|k| {
        (
            None,
            "procgen_param_type".to_string(),
            format!("parameter `{k}` is not RON"),
        )
    })?;
    let merged = match resolve_graph(&generator.graph, &|p| read_graph_text(world, p)) {
        Ok(resolved) => resolved.merged_with(&params),
        Err(_) => params,
    };
    // Overrides of parameters the graph does not declare are dropped (no warnings for them).
    let merged: BTreeMap<String, SettingValue> = merged
        .into_iter()
        .filter(|(k, _)| graph.param(k).is_some())
        .collect();
    let compiled = graph
        .compile_full(&registry, &merged, &|p| read_graph_text(world, p))
        .map_err(|e| {
            let node = e.0.first().and_then(|x| x.node);
            (node, "procgen_graph_invalid".to_string(), e.to_string())
        })?;
    let (snap, _) = capture(world, entity, &compiled);
    let graphs = crate::graph::collect_subgraphs_with(graph, &merged, &|p| read_graph_text(world, p));
    let settings = world.resource::<ProcGenSettings>().clone();
    let mut limits = RunLimits::for_platform();
    if let Some(l) = generator.limits {
        limits = limits.with_override(&l.to_override());
    }
    let inputs = RunInputs {
        world: Arc::new(snap),
        gen_seed: generator_seed(world, entity),
        tier_index: 0,
        cell: None,
        quality: settings.running_tier,
        limits,
        keep_debug: true,
        graphs: Arc::new(graphs),
        terrain_edits: terrain_edit_gate(&generator, &settings),
        run_rect: None,
        tile: None,
    };
    let cache = world.resource::<ProcGenCache>().0.clone();
    Evaluator::with_cache(cache)
        .run(&compiled, &inputs, &CancelToken::new())
        .map_err(|e| match e {
            RunError::Node { node, code, message } => (Some(node), code, message),
            RunError::Cancelled => (None, "cancelled".to_string(), "cancelled".to_string()),
        })
}

#[cfg(test)]
mod cache_budget_tests {
    use super::*;

    /// The editor gets the 512 MiB budget; the runtime keeps its default; an explicit value wins.
    #[test]
    fn editor_cache_budget_is_512_mib() {
        let rt = ProcGenSettings::default();
        assert_eq!(effective_cache_bytes(&rt), rt.cache_bytes);
        let ed = ProcGenSettings {
            editor: true,
            ..ProcGenSettings::default()
        };
        assert_eq!(effective_cache_bytes(&ed), 512 << 20);
        let custom = ProcGenSettings {
            editor: true,
            cache_bytes: 1 << 20,
            ..ProcGenSettings::default()
        };
        assert_eq!(effective_cache_bytes(&custom), 1 << 20);
    }

    /// Audit L15: a collider box through a rotated transform uses the scalar rotation the
    /// evaluator uses (bit for bit), so the capture is the same on every target.
    #[test]
    fn collider_boxes_rotate_with_scalar_math() {
        let q = Quat::from_xyzw(0.183, 0.365, -0.548, 0.730).normalize();
        let tf = Transform {
            translation: Vec3::new(1.5, -2.0, 3.25),
            rotation: q,
            scale: Vec3::new(2.0, 1.0, 0.5),
        };
        let (lo, hi) = (Vec3::new(-1.0, -0.5, -0.25), Vec3::new(1.0, 0.75, 0.5));
        let (got_lo, got_hi) = transform_aabb(&tf, lo, hi);
        let r = crate::rot::Rot(q.to_array());
        let m = r.to_mat3();
        let (c, h) = ((lo + hi) * 0.5 * tf.scale, (hi - lo) * 0.5 * tf.scale.abs());
        let ext = Vec3::new(
            m.x_axis.x.abs() * h.x + m.y_axis.x.abs() * h.y + m.z_axis.x.abs() * h.z,
            m.x_axis.y.abs() * h.x + m.y_axis.y.abs() * h.y + m.z_axis.y.abs() * h.z,
            m.x_axis.z.abs() * h.x + m.y_axis.z.abs() * h.y + m.z_axis.z.abs() * h.z,
        );
        let center = tf.translation + r * c;
        let bits = |v: Vec3| v.to_array().map(f32::to_bits);
        assert_eq!(bits(got_lo), bits(center - ext));
        assert_eq!(bits(got_hi), bits(center + ext));
    }

    /// A generator box with a negative half extent is flat on that axis, matching `PreparedShape`,
    /// so the ground capture rectangle is not inverted.
    #[test]
    fn a_negative_generator_extent_is_clamped_to_zero() {
        use regia_scene::shape::{Shape, ShapeKind};
        let shape = Shape::new(ShapeKind::Box {
            half_extents: Vec3::new(-8.0, 10.0, 8.0),
        });
        let data = world_shape(&shape, &Transform::default());
        let ShapeForm::Box { half_extents } = data.form else {
            panic!("a box shape stays a box");
        };
        assert_eq!(half_extents, [0.0, 10.0, 8.0]);

        let mut world = World::new();
        let entity = world
            .spawn((
                ProcGenerator {
                    bounds: ProcBounds::Box {
                        half_extents: Vec3::new(-8.0, 10.0, 8.0),
                    },
                    ..ProcGenerator::default()
                },
                Transform::default(),
            ))
            .id();
        let bounds = generator_bounds(&world, entity);
        let ShapeForm::Box { half_extents } = bounds.form else {
            panic!("generator bounds stay a box");
        };
        assert_eq!(half_extents, [0.0, 10.0, 8.0]);
        let (lo, hi) = bounds.world_aabb();
        assert!(lo[0] <= hi[0] && lo[2] <= hi[2], "aabb {lo:?} {hi:?}");
    }

    /// corr-low: a collider with an empty (or sub-triple) convex hull has no physics collider
    /// (step.rs refuses < 3 points); the `in.overlap` capture used to invent a part whose
    /// AABB is inverted (`min = +inf`), which never overlaps and can confuse the exact test.
    #[test]
    fn a_degenerate_convex_hull_captures_no_part() {
        use regia_core::math::Vec3;
        use regia_physics::ColliderShape;
        let mut parts = Vec::new();
        shape_parts(&ColliderShape::ConvexHull { points: Vec::new() }, &[], &mut parts);
        assert!(parts.is_empty(), "an empty hull gives no part");
        shape_parts(
            &ColliderShape::ConvexHull {
                points: vec![Vec3::X, Vec3::Y],
            },
            &[],
            &mut parts,
        );
        assert!(parts.is_empty(), "a two-point hull gives no part either");
        shape_parts(
            &ColliderShape::ConvexHull {
                points: vec![Vec3::X, Vec3::Y, Vec3::Z, Vec3::ZERO],
            },
            &[],
            &mut parts,
        );
        assert_eq!(parts.len(), 1, "a real hull still captures");
        assert!(parts[0].min[0] <= parts[0].max[0], "the part's AABB is ordered");
    }
}

#[cfg(test)]
mod stale_snapshot_tests {
    use super::*;

    /// A world with one finished generator that has a tag-query dependency, one ordinary tagged
    /// entity, and one of the generator's own `Generated` outputs (which also carries `Tags`).
    /// Returns `(world, output)`.
    fn world_with_output() -> (World, Entity) {
        let mut world = World::new();
        world.insert_resource(ProcGenSettings::default());
        world.init_resource::<ProcGenWatch>();
        world.spawn((
            EntityGuid(Guid::from_u128(7)),
            ProcGenerator::new("g.pgraph.ron"),
            ProcGenState {
                state: ProcGenRunState::Ready,
                run_id: 1,
                deps: vec![ProcDependency::Query {
                    tags: vec!["rock".to_string()],
                    any_tags: Vec::new(),
                    matched: BTreeSet::new(),
                }],
                ..ProcGenState::default()
            },
        ));
        world.spawn((EntityGuid(Guid::from_u128(8)), Tags::from_iter_str(["rock"])));
        let output = world
            .spawn((
                Generated {
                    generator: Guid::from_u128(7),
                    run_id: 1,
                },
                Tags::from_iter_str(["rock"]),
            ))
            .id();
        (world, output)
    }

    /// A re-run despawns the generator's own `Generated` outputs, which also removes their `Tags`:
    /// those removals are not tag edits and must not invalidate the tag snapshot (the
    /// `Changed<Tags>` query already excludes `Generated`; the removal buffer did not).
    #[test]
    fn despawning_generated_outputs_keeps_the_tag_snapshot() {
        let (mut world, output) = world_with_output();
        crate::probe::arm();
        let before = crate::probe::idle_scans();
        detect_stale(&mut world);
        assert_eq!(
            crate::probe::idle_scans(),
            before + 1,
            "the first scan builds the snapshot"
        );
        world.clear_trackers();
        // A re-run releases the cell: the output despawns and a new one spawns in the same frame,
        // so the entity count is unchanged and only the removal buffer can tell them apart.
        assert!(world.despawn(output), "despawn the output");
        world.spawn_empty();
        detect_stale(&mut world);
        assert_eq!(
            crate::probe::idle_scans(),
            before + 1,
            "despawning a generator's own outputs rebuilt the tag snapshot"
        );
    }

    /// Despawning an ordinary tagged entity still invalidates the snapshot: the `Generated`
    /// exclusion must not swallow real tag removals.
    #[test]
    fn despawning_a_tagged_entity_invalidates_the_tag_snapshot() {
        let (mut world, _) = world_with_output();
        detect_stale(&mut world);
        assert_eq!(
            world.resource::<ProcGenIntern>().tag_snapshot.len(),
            1,
            "the ordinary tagged entity is in the snapshot"
        );
        let rock = world
            .query_filtered::<Entity, (Without<Generated>, With<Tags>)>()
            .iter(&world)
            .next()
            .expect("the tagged entity");
        world.clear_trackers();
        assert!(world.despawn(rock), "despawn the tagged entity");
        world.spawn_empty();
        detect_stale(&mut world);
        assert!(
            world.resource::<ProcGenIntern>().tag_snapshot.is_empty(),
            "the despawned entity stayed in the snapshot"
        );
    }
}
