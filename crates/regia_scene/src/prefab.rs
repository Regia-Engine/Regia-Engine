// SPDX-License-Identifier: MPL-2.0
//! Prefab templates, instancing, overrides and nested prefabs
//! (`docs/specs/14-scene-and-framework.md` A3).
//!
//! A [`PrefabTemplate`] is the **pre-baked** form of a `.prefab.ron`: component values parsed
//! once into reflected values, hierarchy as indices. Instantiating is then a walk that clones the
//! baked values and inserts them through `ReflectComponent` — no file parsing on the hot path.
//! An instance carries [`PrefabInstance`] on its root (which prefab, which overrides); its
//! children carry [`PrefabLink`] with the *local id* (the prefab-internal guid) that overrides
//! address.
//!
//! Nested prefabs compose outer-wins: a prefab file may itself contain prefab instances; baking
//! stores them as [`NestedPrefab`] records, and instantiating recurses, applying the outer
//! overrides after the inner subtree exists. An override target guid resolves against the
//! nearest instantiated subtree that declares it (depth-first from the instance root).

use std::collections::{HashMap, HashSet};

use regia_core::id::Guid;
use regia_ecs::prelude::*;
use regia_ecs::reflect::AppTypeRegistry;
use regia_ecs::serialize::ReflectCodec;

use crate::components::EntityGuid;
use crate::format::{PrefabInstanceData, SceneFile};
use crate::level::{LevelId, LevelMember, Persistent};
use crate::spawn::{SceneSpawnError, SpawnOptions};
use crate::transform::Transform;

/// Safety limit for prefab nesting; a chain this deep is a cycle.
pub const MAX_NESTING_DEPTH: u32 = 64;

/// Work one top-level [`instantiate_prefab`] call may do: entities spawned, component values
/// inserted and instance edits (overrides, added/removed components and children) applied,
/// nested prefabs included. The depth limit alone does not bound fan-out: a prefab whose
/// entities each nest two instances of the next one doubles per level, so a short chain of
/// files would spawn billions of entities. Far above any authored prefab.
const MAX_INSTANTIATION_WORK: usize = 1 << 20;

/// A baked component value, or its raw RON kept because the type was not registered at bake time
/// (retried on every instantiation, so late registration still works).
pub enum BakedValue {
    /// Parsed reflected value.
    Reflected(Box<dyn Reflect>),
    /// Raw RON text and why it could not be parsed.
    Raw(String, String),
}

impl std::fmt::Debug for BakedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reflected(value) => write!(f, "Reflected({})", value.reflect_type_path()),
            Self::Raw(raw, note) => write!(f, "Raw({raw:?}, {note:?})"),
        }
    }
}

/// One entity of a baked prefab.
#[derive(Debug)]
pub struct TemplateEntity {
    /// The prefab-internal local id (the entity's guid in the source file).
    pub local_id: Guid,
    /// The record's `name`: every instance of this entity gets it as a `Name`, inserted before
    /// [`Self::components`] so an explicit `Name` component or override wins (ADR-4928).
    pub name: Option<String>,
    /// Index into [`PrefabTemplate::entities`] of the parent, or `None` for the root.
    pub parent: Option<u32>,
    /// Baked component values in declaration order.
    pub components: Vec<(String, BakedValue)>,
    /// A nested prefab instantiated under this entity.
    pub nested: Option<NestedPrefab>,
}

/// A prefab instance recorded inside a prefab file (nested prefab).
#[derive(Clone, Debug)]
pub struct NestedPrefab {
    /// Which prefab the inner entity instantiates.
    pub asset: Guid,
    /// Overrides authored in the *outer* file; they win over the inner prefab's own values.
    pub instance: PrefabInstanceData,
}

/// The baked, instantiation-ready form of a `.prefab.ron`.
///
/// Build with [`PrefabTemplate::from_scene_file`] and register it in the world's
/// [`PrefabLibrary`]. Baking parses every component value once; [`instantiate_prefab`] then only
/// clones.
#[derive(Debug)]
pub struct PrefabTemplate {
    /// The prefab's asset id.
    pub asset: Guid,
    /// Entities ordered parents-before-children; `entities[0]` is the root.
    pub entities: Vec<TemplateEntity>,
    /// Bake-time warnings (unknown types, lenient fields).
    pub warnings: Vec<String>,
}

impl PrefabTemplate {
    /// Bakes a prefab from its scene file. `asset` is the id the template will be registered
    /// under (normally the file's asset id, not a guid inside it).
    pub fn from_scene_file(
        asset: Guid,
        file: &SceneFile,
        registry: &TypeRegistry,
    ) -> Result<Self, SceneSpawnError> {
        file.validate()?;
        let roots = file.entities.iter().filter(|e| e.parent.is_none()).count();
        if roots != 1 {
            return Err(SceneSpawnError::Invalid(format!(
                "prefab must have exactly one root entity, found {roots}"
            )));
        }

        // Order parents-before-children by resolving iteratively (files may list in any order).
        let mut order: Vec<usize> = Vec::with_capacity(file.entities.len());
        let mut placed = vec![false; file.entities.len()];
        let by_guid: HashMap<Guid, usize> = file
            .entities
            .iter()
            .enumerate()
            .map(|(index, entity)| (entity.guid, index))
            .collect();
        let mut guard = 0;
        while order.len() < file.entities.len() {
            let mut progressed = false;
            for (index, entity) in file.entities.iter().enumerate() {
                if placed[index] {
                    continue;
                }
                let parent_ready = entity
                    .parent
                    .is_none_or(|parent| by_guid.get(&parent).is_some_and(|p| placed[*p]));
                if parent_ready {
                    placed[index] = true;
                    order.push(index);
                    progressed = true;
                }
            }
            if !progressed {
                return Err(SceneSpawnError::Invalid(
                    "prefab hierarchy is not resolvable (cycle or missing parent)".into(),
                ));
            }
            guard += 1;
            if guard > file.entities.len() + 2 {
                return Err(SceneSpawnError::Invalid("prefab ordering loop".into()));
            }
        }

        let index_of_guid = |guid: Guid| by_guid.get(&guid).copied();
        let mut warnings = Vec::new();
        let mut entities = Vec::with_capacity(order.len());
        let mut position: HashMap<usize, u32> = HashMap::new();
        for (position_index, index) in order.iter().enumerate() {
            let record = &file.entities[*index];
            let mut components = Vec::with_capacity(record.components.len());
            for (type_path, raw) in &record.components {
                components.push((
                    type_path.clone(),
                    bake_value(type_path, raw.get_ron(), registry, &mut warnings),
                ));
            }
            let nested = record.prefab_instance.as_ref().map(|instance| NestedPrefab {
                asset: instance.prefab.guid(),
                instance: instance.clone(),
            });
            if let Some(parent) = record.parent
                && index_of_guid(parent).is_none()
            {
                return Err(SceneSpawnError::Invalid(format!(
                    "prefab entity {} references unknown parent {parent}",
                    record.guid
                )));
            }
            position.insert(*index, position_index as u32);
            entities.push(TemplateEntity {
                local_id: record.guid,
                name: record.name.clone(),
                parent: record
                    .parent
                    .and_then(index_of_guid)
                    .and_then(|p| position.get(&p).copied()),
                components,
                nested,
            });
        }

        Ok(Self {
            asset,
            entities,
            warnings,
        })
    }
}

/// Parses one component value at bake time, keeping the raw text when the type is not available.
fn bake_value(type_path: &str, raw: &str, registry: &TypeRegistry, warnings: &mut Vec<String>) -> BakedValue {
    let Some(registration) =
        regia_core::brand::lookup_type_path(type_path, |p| registry.get_with_type_path(p))
    else {
        let note = format!("type `{type_path}` not registered at bake time");
        warnings.push(note.clone());
        return BakedValue::Raw(raw.to_owned(), note);
    };
    if registration.data::<ReflectComponent>().is_none() {
        let note = format!("type `{type_path}` has no ReflectComponent");
        warnings.push(note.clone());
        return BakedValue::Raw(raw.to_owned(), note);
    }
    let mut codec = ReflectCodec::new(registry);
    match codec.from_ron_typed(raw, registration) {
        Ok(value) => {
            // A value written by a newer build is not baked as a live value; the raw text stays
            // so a future build (or the insert-time check) can still refuse it.
            if let Err(err) = crate::spawn::check_format_version(registration, value.as_partial_reflect()) {
                let note = format!("value of `{type_path}` skipped: {err}");
                warnings.push(note.clone());
                return BakedValue::Raw(raw.to_owned(), note);
            }
            BakedValue::Reflected(value)
        }
        Err(err) => {
            let note = format!("value of `{type_path}` failed to parse: {err}");
            warnings.push(note.clone());
            BakedValue::Raw(raw.to_owned(), note)
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime components
// ---------------------------------------------------------------------------

/// Marker on an entity that instantiates a prefab: which prefab and which overrides.
///
/// The overrides and component edits are kept in their **authored** form (raw RON strings), so
/// instances re-serialize exactly what was written. Field ids are `u128` guids (reflection has no
/// `Guid` yet; use the accessors). The instance's added children are not stored here: they are
/// live entities in the instance subtree, and a snapshot writes them back into the record's
/// `added_children` (`crate::spawn::snapshot_scene`).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct PrefabInstance {
    /// The prefab's asset id (see [`PrefabInstance::prefab_guid`]).
    pub prefab: u128,
    /// Field overrides, applied to prefab-internal entities after instantiation.
    pub overrides: Vec<OverrideEntry>,
    /// Prefab-internal local ids (with subtrees) that were not instantiated.
    pub removed_children: Vec<u128>,
    /// Components the instance added to prefab-internal entities.
    pub added_components: Vec<AddedComponentEntry>,
    /// Components the instance removed from prefab-internal entities.
    pub removed_components: Vec<RemovedComponentEntry>,
}

impl PrefabInstance {
    /// The prefab's asset id.
    pub fn prefab_guid(&self) -> Guid {
        Guid::from_u128(self.prefab)
    }

    /// Converts the runtime component back to the file's instance record, without
    /// `added_children` (those are live entities; `crate::spawn::snapshot_scene` collects them).
    ///
    /// An override or added-component value that is no longer valid RON (edited to garbage in
    /// memory) is skipped with a warning rather than silently replaced.
    pub fn to_data(&self) -> PrefabInstanceData {
        let overrides = self
            .overrides
            .iter()
            .filter_map(|entry| match ron::value::RawValue::from_ron(&entry.value) {
                Ok(raw) => Some(crate::format::PrefabOverride {
                    target: Guid::from_u128(entry.target),
                    component: entry.component.clone(),
                    path: entry.path.clone(),
                    value: raw.to_owned(),
                }),
                Err(err) => {
                    tracing::warn!(
                        target: "regia_scene",
                        "dropping override on {} `{}`{}: its value is no longer valid RON: {err}",
                        Guid::from_u128(entry.target),
                        entry.component,
                        entry.path,
                    );
                    None
                }
            })
            .collect();
        let added_components = self
            .added_components
            .iter()
            .filter_map(|entry| match ron::value::RawValue::from_ron(&entry.value) {
                Ok(raw) => Some(crate::format::PrefabComponentAdd {
                    target: Guid::from_u128(entry.target),
                    component: entry.component.clone(),
                    value: raw.to_owned(),
                }),
                Err(err) => {
                    tracing::warn!(
                        target: "regia_scene",
                        "dropping added component `{}` on {}: its value is no longer valid RON: {err}",
                        entry.component,
                        Guid::from_u128(entry.target),
                    );
                    None
                }
            })
            .collect();
        PrefabInstanceData {
            prefab: crate::format::SceneAssetRef(self.prefab_guid()),
            overrides,
            added_components,
            removed_components: self
                .removed_components
                .iter()
                .map(|entry| crate::format::PrefabComponentRemove {
                    target: Guid::from_u128(entry.target),
                    component: entry.component.clone(),
                })
                .collect(),
            added_children: Vec::new(),
            removed_children: self
                .removed_children
                .iter()
                .map(|id| Guid::from_u128(*id))
                .collect(),
        }
    }
}

/// One authored override of a live [`PrefabInstance`].
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
pub struct OverrideEntry {
    /// Target entity's local id (guid within the prefab).
    pub target: u128,
    /// Component type path.
    pub component: String,
    /// Field path (`.intensity`); empty means the whole component.
    pub path: String,
    /// The value as RON text.
    pub value: String,
}

/// One component a live [`PrefabInstance`] added to a prefab-internal entity.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
pub struct AddedComponentEntry {
    /// Target entity's local id (guid within the prefab).
    pub target: u128,
    /// Component type path.
    pub component: String,
    /// The value as RON text.
    pub value: String,
}

/// One component a live [`PrefabInstance`] removed from a prefab-internal entity.
#[derive(Reflect, Clone, Debug, Default, PartialEq)]
pub struct RemovedComponentEntry {
    /// Target entity's local id (guid within the prefab).
    pub target: u128,
    /// Component type path.
    pub component: String,
}

/// Marks entities that belong to a prefab instance subtree, carrying their prefab-internal local
/// id. Instances' overrides address entities through this id.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[reflect(Component, Default)]
pub struct PrefabLink {
    /// The local id (guid within the prefab).
    pub local_id: u128,
}

impl PrefabLink {
    /// The local id as a [`Guid`].
    pub fn guid(&self) -> Guid {
        Guid::from_u128(self.local_id)
    }
}

/// Marks an entity whose prefab template was not loaded yet; filled by
/// [`resolve_pending_prefabs`] once the template (and every prefab nested in it) is registered.
///
/// The original spawn's level context is carried along (`level` 0 = none, `persistent`), so the
/// resolved instance is tagged exactly like a synchronous spawn, and so is the authored instance
/// record (`record`), so the resolved instance keeps its overrides and edits and a snapshot taken
/// while the instance is still pending writes the record back unchanged.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct PendingPrefab {
    /// The awaited prefab asset id.
    pub prefab: u128,
    /// Transform to apply to the instance root when it appears.
    pub root_transform: Option<crate::transform::Transform>,
    /// The owning level's raw id (0 = none; see [`crate::level::LevelMember`]).
    pub level: u128,
    /// Whether the instance is [`crate::level::Persistent`].
    pub persistent: bool,
    /// The authored instance record ([`PrefabInstanceData`]: overrides, component edits, added and
    /// removed children) as RON; empty for a plain instance of `prefab`.
    pub record: String,
}

impl PendingPrefab {
    /// A pending instance of `instance`, carrying the spawn context of `opts`.
    pub fn for_record(
        instance: &PrefabInstanceData,
        root_transform: Option<Transform>,
        opts: &SpawnOptions,
    ) -> Self {
        let record = if *instance == PrefabInstanceData::plain(instance.prefab) {
            String::new()
        } else {
            ron::ser::to_string(instance).unwrap_or_else(|err| {
                tracing::warn!(target: "regia_scene", "pending prefab {}: its record does not serialize ({err}); it will resolve without overrides", instance.prefab);
                String::new()
            })
        };
        Self {
            prefab: instance.prefab.guid().as_u128(),
            root_transform,
            level: opts.level.map(|l| l.0.as_u128()).unwrap_or(0),
            persistent: opts.persistent,
            record,
        }
    }

    /// The instance record to instantiate: the stored one, or a plain instance of `prefab` when
    /// none was stored.
    ///
    /// # Errors
    /// The stored record is not valid RON for [`PrefabInstanceData`] (edited in memory).
    pub fn instance_data(&self) -> Result<PrefabInstanceData, String> {
        if self.record.is_empty() {
            return Ok(PrefabInstanceData::plain(crate::format::SceneAssetRef(
                Guid::from_u128(self.prefab),
            )));
        }
        ron::from_str::<PrefabInstanceData>(&self.record).map_err(|err| err.to_string())
    }
}

/// The world's set of baked prefab templates, keyed by asset id.
///
/// This is the seam to the asset server (`specs/12` § 3): loaders insert templates here when a
/// `.prefab.ron` finishes loading; `PendingPrefab` instances and unloaded levels then complete on
/// the next frame.
///
/// Prefab variants (`docs/specs/83-prefab-profiler-volumes.md` § 2) are registered here too
/// ([`PrefabLibrary::insert_variant`]); an id names either a template or a variant, never both.
#[derive(Resource, Clone, Debug, Default)]
pub struct PrefabLibrary {
    pub(crate) templates: HashMap<Guid, std::sync::Arc<PrefabTemplate>>,
    pub(crate) variants: HashMap<Guid, std::sync::Arc<crate::prefab_variant::PrefabVariant>>,
    /// Set by [`Self::insert`] and [`Self::insert_variant`] to a stamp no other library state
    /// in the process has (see `next_revision`). Pending resolution drops its "known missing" set
    /// when this changes, so a template that arrives is tried again, even in a library that
    /// replaced the resource after the same number of inserts.
    pub(crate) revision: u64,
}

/// A process-wide revision stamp: a per-library counter would repeat when a fresh library
/// replaces the resource and reaches the same insert count.
pub(crate) fn next_revision() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl PrefabLibrary {
    /// An empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a baked template (replacing a variant registered under the same id).
    pub fn insert(&mut self, template: PrefabTemplate) {
        self.revision = next_revision();
        self.variants.remove(&template.asset);
        self.templates
            .insert(template.asset, std::sync::Arc::new(template));
    }

    /// Looks a template up (variants are not templates: see [`Self::variant`]).
    pub fn get(&self, asset: Guid) -> Option<&std::sync::Arc<PrefabTemplate>> {
        self.templates.get(&asset)
    }

    /// Whether `asset` is registered, as a template or as a variant.
    pub fn contains(&self, asset: Guid) -> bool {
        self.templates.contains_key(&asset) || self.variants.contains_key(&asset)
    }

    /// The first template that instantiating `asset` needs but is not registered: `asset` itself,
    /// the base of a variant (any step of its chain) or a prefab nested anywhere inside it. `None`
    /// when the whole nesting is ready. A nesting cycle is not reported here (instantiation
    /// reports it as too deep, or as a variant cycle).
    pub fn first_missing(&self, asset: Guid) -> Option<Guid> {
        let mut stack = vec![asset];
        let mut checked = std::collections::HashSet::new();
        while let Some(asset) = stack.pop() {
            if !checked.insert(asset) {
                continue;
            }
            if let Some(variant) = self.variants.get(&asset) {
                stack.push(variant.base);
                stack.extend(crate::prefab_variant::nested_assets(&variant.edits));
                continue;
            }
            let Some(template) = self.templates.get(&asset) else {
                return Some(asset);
            };
            stack.extend(
                template
                    .entities
                    .iter()
                    .filter_map(|entity| entity.nested.as_ref().map(|nested| nested.asset)),
            );
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Instantiation
// ---------------------------------------------------------------------------

/// The result of instantiating a prefab.
#[derive(Clone, Debug)]
pub struct Instantiation {
    /// The instance root entity (the entity that carries `PrefabInstance`).
    pub root: Entity,
    /// Prefab-internal local id → live entity (includes nested subtrees).
    pub locals: HashMap<Guid, Entity>,
    /// Entities created by this instantiation (root exclusive).
    pub entities_spawned: usize,
    /// Non-fatal problems.
    pub warnings: Vec<String>,
}

impl Default for Instantiation {
    fn default() -> Self {
        Self {
            root: Entity::PLACEHOLDER,
            locals: HashMap::new(),
            entities_spawned: 0,
            warnings: Vec::new(),
        }
    }
}

/// Deterministic guid for a prefab-internal entity: FNV-1a (128-bit) over the instance root's guid
/// and the template-local id, so the same prefab placed under the same root always gets the same
/// guids. Never nil. Public so a tool can map a prefab-internal entity back to the guid it has in
/// another instance of the same prefab (ADR-7331: a placed level's template generator finds the
/// bake its source level wrote).
pub fn derived_guid(root: Guid, local_id: u128) -> Guid {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = OFFSET;
    for byte in root
        .as_u128()
        .to_le_bytes()
        .into_iter()
        .chain(local_id.to_le_bytes())
    {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    Guid::from_u128(hash.max(1))
}

/// Who authored an instance record, which decides how its `added_children` are spawned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordOwner {
    /// The record is level (or caller) data: added children are ordinary scene entities (their
    /// file guid as `EntityGuid`, no [`PrefabLink`]) and a snapshot writes them back.
    Scene,
    /// The record is part of a prefab template (a nested instance): added children belong to the
    /// template like any prefab-internal entity ([`PrefabLink`] with their file guid as local
    /// id, a fresh `EntityGuid`), are addressable by outer overrides, and are never snapshotted.
    Template,
}

/// Instantiates `instance` on the existing entity `root` (which receives [`PrefabInstance`]).
///
/// The record is treated as scene data: its `added_children` become ordinary scene entities
/// that a later `snapshot_scene` writes back into the record. Nested instances inside the
/// template are instantiated as template data.
///
/// Instantiating a template that is not in the [`PrefabLibrary`] fails with
/// [`SceneSpawnError::PrefabNotLoaded`]; callers turn that into a [`PendingPrefab`].
///
/// The root must already carry a non-nil [`EntityGuid`]. Member guids derive from it, so a
/// missing or nil guid fails with [`SceneSpawnError::Invalid`] before anything spawns.
/// `EntityGuid::default` is nil.
pub fn instantiate_prefab(
    world: &mut World,
    instance: &PrefabInstanceData,
    root: Entity,
    opts: &SpawnOptions,
    depth: u32,
) -> Result<Instantiation, SceneSpawnError> {
    let mut budget = MAX_INSTANTIATION_WORK;
    instantiate_prefab_all(
        world,
        instance,
        root,
        opts,
        depth,
        RecordOwner::Scene,
        &mut budget,
    )
    .map(|(result, _)| result)
}

/// Takes `units` of work from an instantiation's budget, or fails once it is spent. A spent
/// budget is left at zero so callers that tolerate a failed nested instance stop as well.
fn charge(budget: &mut usize, units: usize, at: Guid) -> Result<(), SceneSpawnError> {
    if let Some(left) = budget.checked_sub(units) {
        *budget = left;
        return Ok(());
    }
    *budget = 0;
    Err(SceneSpawnError::Invalid(format!(
        "prefab instantiation exceeds {MAX_INSTANTIATION_WORK} entities and edits at {at} (nested prefabs fanning out?)"
    )))
}

/// [`instantiate_prefab`] plus every `(local id, entity)` pair in spawn order. A template that
/// nests the same prefab twice has the inner local ids twice; `Instantiation::locals` keeps the
/// last copy, while overrides and structural edits apply to every copy through this list.
///
/// A failed instantiation (nesting too deep, a bad structural edit, a spent work budget) leaves
/// nothing behind: the entities it spawned under `root` are despawned, so a retry starts clean.
/// `budget` is the work budget shared by the whole nested instantiation (`charge`); `owner` says
/// who authored the record (`RecordOwner`).
fn instantiate_prefab_all(
    world: &mut World,
    instance: &PrefabInstanceData,
    root: Entity,
    opts: &SpawnOptions,
    depth: u32,
    owner: RecordOwner,
    budget: &mut usize,
) -> Result<(Instantiation, Vec<(Guid, Entity)>), SceneSpawnError> {
    let children_before: Vec<Entity> = world
        .get::<Children>(root)
        .map(|children| children.iter().collect())
        .unwrap_or_default();
    let had_instance = world.get::<PrefabInstance>(root).is_some();
    let result = instantiate_prefab_body(world, instance, root, opts, depth, owner, budget);
    if result.is_err() {
        // Template entities hang directly under `root`; their descendants go with them.
        let fresh: Vec<Entity> = world
            .get::<Children>(root)
            .map(|children| {
                children
                    .iter()
                    .filter(|child| !children_before.contains(child))
                    .collect()
            })
            .unwrap_or_default();
        for child in fresh {
            if let Ok(entity) = world.get_entity_mut(child) {
                entity.despawn();
            }
        }
        if !had_instance && let Ok(mut root_entity) = world.get_entity_mut(root) {
            root_entity.remove::<PrefabInstance>();
        }
    }
    result
}

/// The body of `instantiate_prefab_all` (which cleans up after a failure).
fn instantiate_prefab_body(
    world: &mut World,
    instance: &PrefabInstanceData,
    root: Entity,
    opts: &SpawnOptions,
    depth: u32,
    owner: RecordOwner,
    budget: &mut usize,
) -> Result<(Instantiation, Vec<(Guid, Entity)>), SceneSpawnError> {
    let _span = tracing::info_span!("prefab_instantiate", depth).entered();
    if depth > MAX_NESTING_DEPTH {
        return Err(SceneSpawnError::PrefabNestingTooDeep(instance.prefab.guid()));
    }
    let asset = instance.prefab.guid();
    // Check the whole nested chain before spawning anything, so a missing inner template never
    // leaves a half-built outer subtree behind.
    let missing = match world.get_resource::<PrefabLibrary>() {
        Some(library) => library.first_missing(asset),
        None => Some(asset),
    };
    if let Some(missing) = missing {
        return Err(SceneSpawnError::PrefabNotLoaded(missing));
    }
    // A variant resolves to its base template plus the edits of every step, base-most first.
    let crate::prefab_variant::ResolvedPrefab { template, chain } =
        match world.get_resource::<PrefabLibrary>() {
            Some(library) => library.resolve(asset)?,
            None => return Err(SceneSpawnError::PrefabNotLoaded(asset)),
        };
    if world.get_entity(root).is_err() {
        return Err(SceneSpawnError::Invalid(format!(
            "prefab {asset}: instance root {root:?} does not exist"
        )));
    }
    for edits in chain.iter().map(|variant| &variant.edits).chain([instance]) {
        charge(
            budget,
            1 + edits.overrides.len()
                + edits.added_components.len()
                + edits.removed_components.len()
                + edits.removed_children.len(),
            asset,
        )?;
    }
    let registry_handle = world.resource::<AppTypeRegistry>().0.clone();

    let mut result = Instantiation {
        root,
        ..Instantiation::default()
    };
    // A variant step's removed children are pruned like the instance's own.
    let removed: std::collections::HashSet<Guid> = chain
        .iter()
        .flat_map(|variant| variant.edits.removed_children.iter())
        .chain(&instance.removed_children)
        .copied()
        .collect();
    let mut all_locals: Vec<(Guid, Entity)> = Vec::new();

    // Prefab-internal guids derive from the instance root's guid, so reloading the same level
    // yields the same guids. A root without a guid used to fall back to OS-random ones, which
    // diverges between lockstep peers running the same inputs, so the call is refused instead
    // (checked before anything spawns).
    let Some(root_guid) = world.get::<EntityGuid>(root).map(|guid| guid.0) else {
        return Err(SceneSpawnError::Invalid(format!(
            "prefab {asset}: instance root {root:?} has no EntityGuid; set one before instantiating (prefab-internal guids derive from it)"
        )));
    };
    // `EntityGuid` defaults to nil. Two nil roots derive the same member guids.
    if root_guid.is_nil() {
        return Err(SceneSpawnError::Invalid(format!(
            "prefab {asset}: instance root {root:?} has a nil EntityGuid; set a real guid before instantiating (prefab-internal guids derive from it)"
        )));
    }
    let mut spawned_of_index: HashMap<u32, Entity> = HashMap::new();
    for (index, entity_template) in template.entities.iter().enumerate() {
        // `removed_children` prunes whole subtrees (parents come first).
        if removed.contains(&entity_template.local_id) {
            continue;
        }
        if let Some(parent_index) = entity_template.parent
            && !spawned_of_index.contains_key(&parent_index)
        {
            continue; // parent pruned
        }
        charge(budget, 1 + entity_template.components.len(), asset)?;

        let entity_id = world
            .spawn_empty()
            .insert(PrefabLink {
                local_id: entity_template.local_id.as_u128(),
            })
            .insert(EntityGuid(derived_guid(
                root_guid,
                entity_template.local_id.as_u128(),
            )))
            .id();
        // The record name, as `spawn_scene` gives level entities theirs; the baked components
        // (an explicit `Name`) and the overrides below are applied after it.
        if let Some(name) = &entity_template.name {
            world
                .get_entity_mut(entity_id)
                .expect("spawned above") // invariant: entity was spawned above in this call
                .insert(crate::components::Name::new(name.as_str()));
        }
        if let Some(level) = opts.level {
            world
                .get_entity_mut(entity_id)
                .expect("spawned above") // invariant: entity was spawned above in this call
                .insert(LevelMember::from_level(level));
        }
        if opts.persistent {
            world
                .get_entity_mut(entity_id)
                .expect("spawned above") // invariant: entity was spawned above in this call
                .insert(Persistent);
        }

        // Components (registry guard scoped; recursion below must not hold it).
        {
            let registry = registry_handle.read();
            for (type_path, value) in &entity_template.components {
                match value {
                    BakedValue::Reflected(value) => {
                        insert_reflected(
                            world,
                            entity_id,
                            type_path,
                            value.as_ref(),
                            &registry,
                            &mut result.warnings,
                        );
                    }
                    BakedValue::Raw(raw, note) => {
                        // Retry: the type may have been registered since baking.
                        let parsed = regia_core::brand::lookup_type_path(type_path.as_str(), |p| {
                            registry.get_with_type_path(p)
                        })
                        .map(|registration| {
                            ReflectCodec::new(&registry).from_ron_typed(raw.as_str(), registration)
                        });
                        match parsed {
                            Some(Ok(value)) => insert_reflected(
                                world,
                                entity_id,
                                type_path,
                                value.as_ref(),
                                &registry,
                                &mut result.warnings,
                            ),
                            Some(Err(err)) => result.warnings.push(format!(
                                "prefab {asset}: component {type_path} on {} failed to load: {err} (bake note: {note})",
                                entity_template.local_id
                            )),
                            None => result.warnings.push(format!(
                                "prefab {asset}: component {type_path} on {} is still unregistered (bake note: {note})",
                                entity_template.local_id
                            )),
                        }
                    }
                }
            }
        }

        result.locals.insert(entity_template.local_id, entity_id);
        all_locals.push((entity_template.local_id, entity_id));
        spawned_of_index.insert(index as u32, entity_id);
        result.entities_spawned += 1;

        // Parent: template root hangs under the instance root; others under their template parent.
        let parent = entity_template
            .parent
            .and_then(|parent_index| spawned_of_index.get(&parent_index).copied())
            .unwrap_or(root);
        world
            .get_entity_mut(entity_id)
            .expect("spawned above") // invariant: entity was spawned above in this call
            .insert(ChildOf(parent));

        // Nested prefab: this entity is the inner instance root; the inner subtree spawns
        // directly under it (outer components stay on the entity; inner overrides, then the
        // outer instance's overrides, apply after).
        if let Some(nested) = &entity_template.nested {
            // A nested record is template data (its added children are prefab-internal). On error
            // the enclosing `instantiate_prefab_all` despawns everything this call spawned.
            let (nested_result, nested_all) = instantiate_prefab_all(
                world,
                &nested.instance,
                entity_id,
                opts,
                depth + 1,
                RecordOwner::Template,
                budget,
            )?;
            all_locals.extend(nested_all);
            result.entities_spawned += nested_result.entities_spawned;
            for (local, entity) in nested_result.locals {
                result.locals.insert(local, entity);
            }
            result.warnings.extend(nested_result.warnings);
        }
    }

    // `removed_children` also names entities that exist only inside nested templates; prune
    // those subtrees now that the nested spawns are done.
    if !removed.is_empty() {
        let doomed: Vec<Entity> = all_locals
            .iter()
            .filter(|(local, _)| removed.contains(local))
            .map(|(_, entity)| *entity)
            .collect();
        for entity in doomed {
            if world.get_entity(entity).is_err() {
                continue; // already gone with an earlier pruned ancestor
            }
            let count = subtree_size(world, entity);
            if crate::hierarchy::despawn_tree(world, entity) {
                result.entities_spawned = result.entities_spawned.saturating_sub(count);
            }
        }
        all_locals.retain(|(_, entity)| world.get_entity(*entity).is_ok());
        result
            .locals
            .retain(|_, entity| world.get_entity(*entity).is_ok());
    }

    // Variant steps, base-most first: each is a template-owned instance record over the base's
    // subtree (its added children are prefab-internal and addressable by later steps and by the
    // instance), applied before the instance's own record so the instance wins.
    for variant in &chain {
        let registry = registry_handle.read();
        let mut ctx = OverrideCtx {
            opts,
            depth,
            owner: RecordOwner::Template,
            root_guid,
            registry: &registry,
            warnings: &mut result.warnings,
            budget: &mut *budget,
        };
        let added =
            apply_structural_edits(world, &variant.edits, root, &result.locals, &all_locals, &mut ctx)?;
        result.entities_spawned += added.len();
        all_locals.extend(added.iter().map(|(local, entity)| (*local, *entity)));
        result.locals.extend(added);
        apply_record_overrides(world, &variant.edits, &all_locals, variant.asset, &mut ctx);
    }

    // Bookkeeping on the instance root.
    let Ok(mut root_entity) = world.get_entity_mut(root) else {
        return Err(SceneSpawnError::Invalid(format!(
            "prefab {asset}: instance root {root:?} vanished during instantiation"
        )));
    };
    root_entity.insert(PrefabInstance {
        prefab: asset.as_u128(),
        overrides: instance
            .overrides
            .iter()
            .map(|o| OverrideEntry {
                target: o.target.as_u128(),
                component: o.component.clone(),
                path: o.path.clone(),
                value: o.value.get_ron().to_owned(),
            })
            .collect(),
        added_components: instance
            .added_components
            .iter()
            .map(|add| AddedComponentEntry {
                target: add.target.as_u128(),
                component: add.component.clone(),
                value: add.value.get_ron().to_owned(),
            })
            .collect(),
        removed_components: instance
            .removed_components
            .iter()
            .map(|remove| RemovedComponentEntry {
                target: remove.target.as_u128(),
                component: remove.component.clone(),
            })
            .collect(),
        removed_children: instance.removed_children.iter().map(|g| g.as_u128()).collect(),
    });

    // Structural edits against the fully spawned subtree, then field overrides last:
    // outer overrides apply over everything the prefab set up.
    {
        let registry = registry_handle.read();
        let mut ctx = OverrideCtx {
            opts,
            depth,
            owner,
            root_guid,
            registry: &registry,
            warnings: &mut result.warnings,
            budget,
        };
        // On error `instantiate_prefab_all` removes what this call spawned and the instance record.
        let added = apply_structural_edits(world, instance, root, &result.locals, &all_locals, &mut ctx)?;
        // Template-owned added children are prefab-internal: outer overrides may address them.
        if owner == RecordOwner::Template {
            result.entities_spawned += added.len();
            all_locals.extend(added.iter().map(|(local, entity)| (*local, *entity)));
            result.locals.extend(added);
        } else {
            // A scene-owned instance's added children are not addressable by overrides, but a
            // scene entity may be parented to one (`validate` accepts that), so the parent pass
            // of `spawn_scene` must find them.
            result.locals.extend(added);
        }
        apply_record_overrides(world, instance, &all_locals, asset, &mut ctx);
    }

    Ok((result, all_locals))
}

/// Applies the field overrides of one instance record (or variant step) to every live entity of
/// each target local id; a missing target or a failed override is a warning naming `asset`.
fn apply_record_overrides(
    world: &mut World,
    record: &PrefabInstanceData,
    all_locals: &[(Guid, Entity)],
    asset: Guid,
    ctx: &mut OverrideCtx<'_>,
) {
    for entry in &record.overrides {
        let targets: Vec<Entity> = targets_of(all_locals, entry.target).collect();
        if targets.is_empty() {
            ctx.warnings.push(format!(
                "prefab {asset}: override target {} not found in the instantiated subtree",
                entry.target
            ));
            continue;
        }
        for target in targets {
            if let Err(err) = apply_override(
                world,
                target,
                &entry.component,
                &entry.path,
                entry.value.get_ron(),
                ctx.registry,
            ) {
                ctx.warnings
                    .push(format!("prefab {asset}: override failed: {err}"));
            }
        }
    }
}

/// Every live entity spawned for local id `target` (more than one when a template nests the
/// same prefab twice).
fn targets_of(all_locals: &[(Guid, Entity)], target: Guid) -> impl Iterator<Item = Entity> + '_ {
    all_locals
        .iter()
        .filter(move |(local, _)| *local == target)
        .map(|(_, entity)| *entity)
}

/// `entity` plus all of its descendants.
fn subtree_size(world: &World, entity: Entity) -> usize {
    let mut count = 0;
    let mut stack = vec![entity];
    // `ChildOf` does not reject cycles, so `Children` may loop: count each entity once.
    let mut seen: std::collections::HashSet<Entity> = std::collections::HashSet::new();
    while let Some(next) = stack.pop() {
        if !seen.insert(next) {
            continue;
        }
        count += 1;
        if let Some(children) = world.get::<Children>(next) {
            stack.extend(children.iter());
        }
    }
    count
}

/// [`Commands`]/`World` extensions for spawning prefab instances (spec A3's `spawn_prefab`).
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_core::id::Guid;
/// use regia_scene::prelude::*;
///
/// let mut app = App::new();
/// app.add_plugins(ScenePlugin);
///
/// // Bake a one-entity prefab from a `.prefab.ron` string and register it.
/// let root = Guid::new_random();
/// let file = SceneFile::from_ron_str(&format!(
///     r#"(format_version: 1, entities: [
///         (guid: "{root}", name: "Lamp", components: {{
///             "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
///         }}),
///     ])"#
/// ))
/// .expect("valid prefab RON");
/// let asset = Guid::new_random();
/// let registry = app.world().resource::<regia_ecs::reflect::AppTypeRegistry>().0.clone();
/// let template =
///     regia_scene::prefab::PrefabTemplate::from_scene_file(asset, &file, &registry.read())
///         .expect("bake");
/// app.world_mut().resource_mut::<PrefabLibrary>().insert(template);
///
/// // Spawn an instance: the root carries `PrefabInstance`, and the template's single child
/// // is parented underneath it.
/// let instance = app
///     .world_mut()
///     .spawn_prefab(asset, None, &SpawnOptions::default(), Guid::new_random())
///     .expect("spawn");
/// assert!(app.world().get::<PrefabInstance>(instance).is_some());
/// assert_eq!(app.world().get::<Children>(instance).map(|c| c.len()), Some(1));
/// ```
pub trait PrefabWorldExt {
    /// Spawns a new entity that instantiates `prefab` (spec A3's `spawn_prefab`).
    ///
    /// The entity is created immediately with `EntityGuid(root_guid)`; the caller owns guid
    /// allocation so lockstep peers can draw it from the same sim RNG and derive identical
    /// prefab-internal guids (see [`instantiate_prefab`]). `root_guid` must not be
    /// [`Guid::NIL`] and must not belong to an entity already in the world.
    ///
    /// When the template is already in the [`PrefabLibrary`] the subtree is instantiated right
    /// away; otherwise the entity carries [`PendingPrefab`] (plus `transform` for when it
    /// resolves) and [`resolve_pending_prefabs`] fills it on a later frame — that is the async
    /// seam (`specs/14` A3).
    ///
    /// # Errors
    /// Returns the error of [`instantiate_prefab`] (other than
    /// [`SceneSpawnError::PrefabNotLoaded`], which becomes [`PendingPrefab`]), or
    /// [`SceneSpawnError::Invalid`] when `root_guid` is nil or already present.
    fn spawn_prefab(
        &mut self,
        prefab: Guid,
        transform: Option<Transform>,
        opts: &SpawnOptions,
        root_guid: Guid,
    ) -> Result<Entity, SceneSpawnError>;
}

impl PrefabWorldExt for World {
    fn spawn_prefab(
        &mut self,
        prefab: Guid,
        transform: Option<Transform>,
        opts: &SpawnOptions,
        root_guid: Guid,
    ) -> Result<Entity, SceneSpawnError> {
        if root_guid.is_nil() {
            return Err(SceneSpawnError::Invalid(format!(
                "prefab {prefab}: the root guid must not be nil"
            )));
        }
        let duplicate = {
            let mut query = self.query::<&EntityGuid>();
            query.iter(self).any(|guid| guid.0 == root_guid)
        };
        if duplicate {
            return Err(SceneSpawnError::Invalid(format!(
                "prefab {prefab}: an entity with guid {root_guid} already exists"
            )));
        }
        let entity = self.spawn_empty().insert(EntityGuid(root_guid)).id();
        if let Some(level) = opts.level {
            self.get_entity_mut(entity)
                .expect("spawned above") // invariant: entity was spawned above in this call
                .insert(LevelMember::from_level(level));
        }
        if opts.persistent {
            self.get_entity_mut(entity)
                .expect("spawned above") // invariant: entity was spawned above in this call
                .insert(Persistent);
        }
        if let Some(transform) = transform {
            self.get_entity_mut(entity)
                .expect("spawned above") // invariant: entity was spawned above in this call
                .insert(transform);
        }
        let instance = PrefabInstanceData::plain(crate::format::SceneAssetRef(prefab));
        match instantiate_prefab(self, &instance, entity, opts, 0) {
            Ok(_) => Ok(entity),
            Err(SceneSpawnError::PrefabNotLoaded(_)) => {
                self.get_entity_mut(entity)
                    .expect("spawned above") // invariant: entity was spawned above in this call
                    .insert(PendingPrefab::for_record(&instance, transform, opts));
                Ok(entity)
            }
            Err(err) => {
                // Nothing usable was built; do not leave an empty husk behind.
                if !crate::hierarchy::despawn_tree(self, entity) {
                    tracing::warn!(target: "regia_scene", "spawn_prefab: the failed instance {entity:?} was already gone");
                }
                Err(err)
            }
        }
    }
}

/// [`Commands`] extension: `commands.spawn_prefab(prefab, transform, opts, root_guid)` — the
/// deferred form of [`PrefabWorldExt::spawn_prefab`] (the caller owns the root guid; see there).
/// The result (entity or error) is reported through `tracing`.
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_core::id::Guid;
/// use regia_scene::prelude::*;
///
/// let mut app = App::new();
/// app.add_plugins(ScenePlugin);
///
/// // Queue a spawn through `Commands` from inside a system.
/// app.add_systems(Update, |mut commands: Commands| {
///     commands.spawn_prefab(Guid::new_random(), None, &SpawnOptions::default(), Guid::new_random());
/// });
/// app.update();
///
/// // The template is not loaded yet, so the instance is a `PendingPrefab` on its root.
/// let pending = {
///     let world = app.world_mut();
///     world.query::<(Entity, &PendingPrefab)>().iter(world).count()
/// };
/// assert_eq!(pending, 1);
/// ```
pub trait PrefabCommands {
    /// Queues a [`PrefabWorldExt::spawn_prefab`] call (see there for semantics and the async
    /// behaviour).
    fn spawn_prefab(
        &mut self,
        prefab: Guid,
        transform: Option<Transform>,
        opts: &SpawnOptions,
        root_guid: Guid,
    ) -> &mut Self;
}

impl PrefabCommands for Commands<'_, '_> {
    fn spawn_prefab(
        &mut self,
        prefab: Guid,
        transform: Option<Transform>,
        opts: &SpawnOptions,
        root_guid: Guid,
    ) -> &mut Self {
        let opts = opts.clone();
        self.queue(move |world: &mut World| {
            if let Err(err) = world.spawn_prefab(prefab, transform, &opts, root_guid) {
                tracing::warn!(target: "regia_scene", "spawn_prefab {prefab}: {err}");
            }
        });
        self
    }
}

/// Inserts a reflected component value (or warns when the type lost its registration).
fn insert_reflected(
    world: &mut World,
    entity: Entity,
    type_path: &str,
    value: &dyn Reflect,
    registry: &TypeRegistry,
    warnings: &mut Vec<String>,
) {
    let Some(registration) =
        regia_core::brand::lookup_type_path(type_path, |p| registry.get_with_type_path(p))
    else {
        warnings.push(format!("component {type_path} is not registered"));
        return;
    };
    let Some(reflect_component) = registration.data::<ReflectComponent>() else {
        warnings.push(format!("component {type_path} has no ReflectComponent"));
        return;
    };
    if let Err(err) = crate::spawn::check_format_version(registration, value.as_partial_reflect()) {
        warnings.push(format!("component {type_path} skipped: {err}"));
        return;
    }
    if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
        reflect_component.insert(&mut entity_mut, value, registry);
    }
}

/// Borrows for one instance's structural edits and overrides.
struct OverrideCtx<'a> {
    opts: &'a SpawnOptions,
    depth: u32,
    owner: RecordOwner,
    /// The instance root's guid; template-owned added children derive their guids from it.
    root_guid: Guid,
    registry: &'a TypeRegistry,
    warnings: &'a mut Vec<String>,
    /// The enclosing top-level instantiation's remaining work (see `MAX_INSTANTIATION_WORK`).
    budget: &'a mut usize,
}

/// Applies `added_components`, `removed_components` and `added_children` of an instance record.
/// Returns the added children's entities by their record guid.
fn apply_structural_edits(
    world: &mut World,
    instance: &PrefabInstanceData,
    instance_root: Entity,
    locals: &HashMap<Guid, Entity>,
    all_locals: &[(Guid, Entity)],
    ctx: &mut OverrideCtx<'_>,
) -> Result<HashMap<Guid, Entity>, SceneSpawnError> {
    for add in &instance.added_components {
        let targets: Vec<Entity> = targets_of(all_locals, add.target).collect();
        if targets.is_empty() {
            ctx.warnings
                .push(format!("added component target {} not found", add.target));
            continue;
        }
        for target in targets {
            if let Err(err) = apply_override(
                world,
                target,
                &add.component,
                "",
                add.value.get_ron(),
                ctx.registry,
            ) {
                ctx.warnings.push(format!("added component failed: {err}"));
            }
        }
    }

    for remove in &instance.removed_components {
        let targets: Vec<Entity> = targets_of(all_locals, remove.target).collect();
        if targets.is_empty() {
            ctx.warnings
                .push(format!("removed component target {} not found", remove.target));
            continue;
        }
        let reflect_component = regia_core::brand::lookup_type_path(remove.component.as_str(), |p| {
            ctx.registry.get_with_type_path(p)
        })
        .and_then(|registration| registration.data::<ReflectComponent>());
        let Some(reflect_component) = reflect_component else {
            ctx.warnings.push(format!(
                "removed component: type {} is not registered or has no ReflectComponent",
                remove.component
            ));
            continue;
        };
        for target in targets {
            if let Ok(mut entity) = world.get_entity_mut(target) {
                reflect_component.remove(&mut entity);
            }
        }
    }

    // Added children: entity subtrees attached into the instance. Their `parent` may point at a
    // prefab-internal entity, at another added child, or default to the instance root; spawn
    // every record first, then parent, so references between added children resolve. The
    // records are not checked against each other on load, so a link that would close a parent
    // cycle between added children falls back to the instance root.
    enum Target {
        Local(Entity),
        Added(usize),
        Missing(Guid),
        Root,
    }
    let mut spawned_children: Vec<Entity> = Vec::with_capacity(instance.added_children.len());
    let mut added_index: HashMap<Guid, usize> = HashMap::with_capacity(instance.added_children.len());
    for (index, child) in instance.added_children.iter().enumerate() {
        match spawn_added_child(world, child, ctx) {
            Ok(entity) => spawned_children.push(entity),
            Err(err) => {
                // Not parented yet, so the instance root's despawn would not reach them.
                for entity in &spawned_children {
                    if let Ok(entity_mut) = world.get_entity_mut(*entity) {
                        entity_mut.despawn();
                    }
                }
                return Err(err);
            }
        }
        added_index.insert(child.guid, index);
    }
    let targets: Vec<Target> = instance
        .added_children
        .iter()
        .map(|child| match child.parent {
            None => Target::Root,
            Some(guid) => match (locals.get(&guid), added_index.get(&guid)) {
                (Some(local), _) => Target::Local(*local),
                (None, Some(index)) => Target::Added(*index),
                (None, None) => Target::Missing(guid),
            },
        })
        .collect();
    let links: Vec<Option<usize>> = targets
        .iter()
        .map(|target| match target {
            Target::Added(index) => Some(*index),
            _ => None,
        })
        .collect();
    let closes_cycle = crate::spawn::cycle_closing_links(&links);
    for (index, (child, target)) in instance.added_children.iter().zip(&targets).enumerate() {
        let entity = spawned_children[index];
        let parent = match target {
            Target::Local(parent) => *parent,
            Target::Added(_) if closes_cycle[index] => {
                ctx.warnings.push(format!(
                    "added child {}: parent link would close a parent cycle; using the instance root",
                    child.guid
                ));
                instance_root
            }
            Target::Added(other) => spawned_children[*other],
            Target::Missing(parent_guid) => {
                ctx.warnings.push(format!(
                    "added child {}: parent {parent_guid} not found; using the instance root",
                    child.guid
                ));
                instance_root
            }
            Target::Root => instance_root,
        };
        if parent != entity
            && let Ok(mut entity_mut) = world.get_entity_mut(entity)
        {
            entity_mut.insert(ChildOf(parent));
        }
    }
    Ok(instance
        .added_children
        .iter()
        .map(|child| child.guid)
        .zip(spawned_children)
        .collect())
}

/// Spawns one `added_children` record (unparented; the caller parents it). A record that itself
/// instantiates a prefab gets that subtree under it (depth-guarded like any nested prefab).
fn spawn_added_child(
    world: &mut World,
    record: &crate::format::SceneEntity,
    ctx: &mut OverrideCtx<'_>,
) -> Result<Entity, SceneSpawnError> {
    charge(ctx.budget, 1 + record.components.len(), record.guid)?;
    let mut builder = world.spawn_empty();
    match ctx.owner {
        RecordOwner::Scene => {
            builder.insert(EntityGuid(record.guid));
        }
        RecordOwner::Template => {
            // Prefab-internal: the file guid is the local id; every instance gets its own
            // `EntityGuid` (two instances of the prefab must not share one), derived from the
            // instance root's guid like the template's own entities.
            let guid = derived_guid(ctx.root_guid, record.guid.as_u128());
            builder.insert((
                PrefabLink {
                    local_id: record.guid.as_u128(),
                },
                EntityGuid(guid),
            ));
        }
    }
    if let Some(name) = &record.name {
        builder.insert(crate::components::Name::new(name.as_str()));
    }
    if let Some(level) = ctx.opts.level {
        builder.insert(LevelMember::from_level(level));
    }
    if ctx.opts.persistent {
        builder.insert(Persistent);
    }
    let entity_id = builder.id();
    let mut entity_mut = world.get_entity_mut(entity_id).expect("spawned above"); // invariant: spawned above
    for (type_path, raw) in &record.components {
        let Some(registration) =
            regia_core::brand::lookup_type_path(type_path.as_str(), |p| ctx.registry.get_with_type_path(p))
        else {
            ctx.warnings
                .push(format!("added child: type {type_path} not registered"));
            continue;
        };
        let Some(reflect_component) = registration.data::<ReflectComponent>() else {
            ctx.warnings
                .push(format!("added child: type {type_path} has no ReflectComponent"));
            continue;
        };
        let decoded = ReflectCodec::new(ctx.registry)
            .from_ron_typed(raw.get_ron(), registration)
            .map_err(|err| err.to_string())
            .and_then(|value| {
                crate::spawn::check_format_version(registration, value.as_partial_reflect())?;
                Ok(value)
            });
        // A component that fails to decode (or was written by a newer build) is skipped with a
        // warning, exactly like a top-level entity's component: one bad component must not abort
        // the whole spawn. The hard failure stays for a spent work budget (above and for nested
        // prefabs below).
        match decoded {
            Ok(value) => reflect_component.insert(&mut entity_mut, &*value, ctx.registry),
            Err(err) => ctx.warnings.push(format!(
                "added child {}: component {type_path} failed to load: {err}; skipped",
                record.guid
            )),
        }
    }

    if let Some(instance) = &record.prefab_instance {
        match instantiate_prefab_all(
            world,
            instance,
            entity_id,
            ctx.opts,
            ctx.depth + 1,
            ctx.owner,
            ctx.budget,
        ) {
            Ok(_) => {}
            Err(SceneSpawnError::PrefabNotLoaded(_)) => {
                // Async seam, like a top-level instance: fill when the template arrives.
                if let Ok(mut entity_mut) = world.get_entity_mut(entity_id) {
                    entity_mut.insert(PendingPrefab::for_record(instance, None, ctx.opts));
                }
                ctx.warnings.push(format!(
                    "added child {}: prefab {} not loaded; instance is pending",
                    record.guid,
                    instance.prefab.guid()
                ));
            }
            // A spent work budget ends the whole instantiation; anything else only skips this
            // subtree. The record is not parented yet, so it goes here.
            Err(err) if *ctx.budget == 0 => {
                if let Ok(entity_mut) = world.get_entity_mut(entity_id) {
                    entity_mut.despawn();
                }
                return Err(err);
            }
            Err(err) => ctx
                .warnings
                .push(format!("added child: nested prefab failed: {err}")),
        }
    }

    Ok(entity_id)
}

/// Applies one override: `path` `""` replaces the whole component, else patches the field at
/// `path` (leading-dot normalized, e.g. `.intensity`).
pub fn apply_override(
    world: &mut World,
    target: Entity,
    component_type: &str,
    path: &str,
    value_ron: &str,
    registry: &TypeRegistry,
) -> Result<(), String> {
    let registration =
        regia_core::brand::lookup_type_path(component_type, |p| registry.get_with_type_path(p))
            .ok_or_else(|| format!("component type `{component_type}` is not registered"))?;
    let reflect_component = registration
        .data::<ReflectComponent>()
        .ok_or_else(|| format!("component `{component_type}` has no ReflectComponent"))?;

    let normalized_path = normalize_path(path);
    let mut codec = ReflectCodec::new(registry);
    if normalized_path.is_empty() {
        // Whole-component replacement.
        let value = codec
            .from_ron_typed(value_ron, registration)
            .map_err(|err| err.to_string())
            .and_then(|value| {
                crate::spawn::check_format_version(registration, value.as_partial_reflect())?;
                Ok(value)
            })?;
        let Ok(mut entity) = world.get_entity_mut(target) else {
            return Err(format!("target entity {target:?} does not exist"));
        };
        reflect_component.insert(&mut entity, &*value, registry);
        return Ok(());
    }

    // Field patch: deserialize against the field's own type, then apply.
    let Ok(entity) = world.get_entity_mut(target) else {
        return Err(format!("target entity {target:?} does not exist"));
    };
    let Some(current) = reflect_component.reflect(&entity) else {
        return Err(format!("entity {target:?} has no `{component_type}` to override"));
    };
    // Discover the field's type from the current value.
    let field_info = current.reflect_path(normalized_path).ok().and_then(|field| {
        field
            .get_represented_type_info()
            .map(|info| info.type_path().to_owned())
    });
    let Some(field_type_path) = field_info else {
        return Err(format!(
            "cannot resolve `{normalized_path}` on `{component_type}` (or its type is opaque)"
        ));
    };
    let field_registration = registry.get_with_type_path(field_type_path.as_str()).cloned();
    let Some(field_registration) = field_registration else {
        return Err(format!(
            "override field type `{field_type_path}` is not registered; register it to override `{normalized_path}`"
        ));
    };
    let field_value = codec
        .from_ron_typed(value_ron, &field_registration)
        .map_err(|err| err.to_string())?;
    // Re-borrow mutably now that the immutable borrow ended.
    let Ok(mut entity) = world.get_entity_mut(target) else {
        return Err(format!("target entity {target:?} does not exist"));
    };
    let Some(mut current) = reflect_component.reflect_mut(&mut entity) else {
        return Err(format!(
            "entity {target:?} lost its `{component_type}` during override"
        ));
    };
    current
        .reflect_path_mut(normalized_path)
        .map_err(|err| err.to_string())?
        .try_apply(field_value.as_partial_reflect())
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// `".intensity"` / `"intensity"` → `"intensity"`; `""` / `"."` → `""` (whole component).
fn normalize_path(path: &str) -> &str {
    if path.is_empty() || path == "." {
        return "";
    }
    path.strip_prefix('.').unwrap_or(path)
}

/// Query and the set of prefab assets already known to be missing, so a frame that is still
/// waiting does not build a `QueryState`, clone instance records, or walk the nest again.
#[derive(Resource, Default)]
struct PendingResolveCache {
    query: Option<QueryState<Entity, With<PendingPrefab>>>,
    ids: Vec<Entity>,
    missing: HashSet<Guid>,
    /// Library revision last time `missing` was cleared. `None` when the world had no library.
    revision: Option<u64>,
}

/// Fills [`PendingPrefab`] instances whose template (with every nested prefab) has arrived, with
/// the instance record and spawn context the pending entity recorded (overrides and edits, level
/// membership, persistence, root transform). An instance that still misses a template stays
/// pending. The record string is copied only for an instance that will spawn this frame.
#[tracing::instrument(target = "regia_scene", skip_all)]
pub fn resolve_pending_prefabs(world: &mut World) {
    let _span = tracing::debug_span!("resolve_pending_prefabs").entered();
    let mut cache = world.remove_resource::<PendingResolveCache>().unwrap_or_default();
    if cache.query.is_none() {
        cache.query = Some(QueryState::new(world));
    }
    cache.ids.clear();
    if let Some(query) = cache.query.as_mut() {
        cache.ids.extend(query.iter(world));
    }
    // Query order depends on archetype creation order; spawn in entity order instead.
    cache.ids.sort_unstable();
    let revision = world
        .get_resource::<PrefabLibrary>()
        .map(|library| library.revision);
    if cache.revision != revision {
        cache.missing.clear();
        cache.revision = revision;
    }
    let mut ready = Vec::new();
    for entity in cache.ids.iter().copied() {
        let Some(prefab) = world.get::<PendingPrefab>(entity).map(|pending| pending.prefab) else {
            continue;
        };
        let asset = Guid::from_u128(prefab);
        if cache.missing.contains(&asset) {
            continue;
        }
        let known = world
            .get_resource::<PrefabLibrary>()
            .is_some_and(|library| library.first_missing(asset).is_none());
        if known {
            ready.push(entity);
        } else {
            cache.missing.insert(asset);
        }
    }
    world.insert_resource(cache);
    for entity in ready {
        let Some(pending) = world.get::<PendingPrefab>(entity).cloned() else {
            continue;
        };
        let asset = Guid::from_u128(pending.prefab);
        let data = pending.instance_data().unwrap_or_else(|err| {
            tracing::warn!(target: "regia_scene", "pending prefab {asset} on {entity:?}: its record does not parse ({err}); resolving without overrides");
            PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        });
        let opts = SpawnOptions {
            level: (pending.level != 0).then_some(LevelId(Guid::from_u128(pending.level))),
            persistent: pending.persistent,
            ..SpawnOptions::default()
        };
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            continue;
        };
        entity_mut.remove::<PendingPrefab>();
        if let Some(transform) = pending.root_transform {
            entity_mut.insert(transform);
        }
        // The recorded spawn context tags the root itself too (a synchronous spawn inserts
        // these at creation; the subtree is tagged by `instantiate_prefab` below).
        if let Some(level) = opts.level {
            entity_mut.insert(LevelMember::from_level(level));
        }
        if opts.persistent {
            entity_mut.insert(Persistent);
        }
        match instantiate_prefab(world, &data, entity, &opts, 0) {
            Ok(_) => {}
            Err(SceneSpawnError::PrefabNotLoaded(missing)) => {
                // A template went missing between the check and the spawn (or an added child's
                // nested prefab is unloaded): nothing was left half-built, so wait again.
                tracing::debug!(target: "regia_scene", "pending prefab {asset} on {entity:?} still waits for {missing}");
                if let Ok(mut entity_mut) = world.get_entity_mut(entity) {
                    entity_mut.insert(pending);
                }
            }
            Err(err) => {
                tracing::warn!(target: "regia_scene", "pending prefab {asset} on {entity:?} could not resolve: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Name, Tags};
    use crate::format::{PrefabComponentRemove, PrefabOverride};
    use crate::propagate::propagate_transforms;
    use crate::transform::{GlobalTransform, Transform};
    use regia_core::math::Vec3;
    use smallvec::SmallVec;

    mod test_components {
        use regia_ecs::prelude::*;

        /// Point light stand-in with an intensity field, for override tests.
        #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
        #[reflect(Component, Default)]
        pub struct PointLight {
            /// Radiant intensity in candelas.
            pub intensity: f32,
            /// Light colour.
            pub color: Vec3Adapter,
        }

        /// Colour wrapper so the struct has a nested field to patch.
        #[derive(Reflect, Clone, Copy, Debug, Default, PartialEq)]
        pub struct Vec3Adapter {
            /// Red.
            pub x: f32,
            /// Green.
            pub y: f32,
            /// Blue.
            pub z: f32,
        }
    }

    /// Spec 14 A3: the `Commands` form takes the same four arguments, returns `&mut Self` so
    /// spawns chain, and on an error (nil or already-used root guid) only logs: nothing is
    /// spawned for the refused calls and the valid call in the same chain still lands.
    #[test]
    fn commands_spawn_prefab_refuses_bad_root_guids_and_keeps_chaining() {
        use regia_app::prelude::*;
        let prefab = Guid::new_random();
        let good = Guid::new_random();
        let mut app = App::new();
        app.add_plugins(crate::ScenePlugin);
        app.add_systems(Update, move |mut commands: Commands| {
            commands
                .spawn_prefab(prefab, None, &SpawnOptions::default(), Guid::NIL)
                .spawn_prefab(prefab, None, &SpawnOptions::default(), good)
                .spawn_prefab(prefab, None, &SpawnOptions::default(), good);
        });
        app.update();
        let world = app.world_mut();
        let guids: Vec<Guid> = world.query::<&EntityGuid>().iter(world).map(|g| g.0).collect();
        assert_eq!(guids, vec![good], "only the valid call spawned a root");
        let pending = world.query::<&PendingPrefab>().iter(world).count();
        assert_eq!(
            pending, 1,
            "the template is not loaded, so the one root waits for it"
        );
    }

    fn registry_with() -> (World, Guid, Guid, Guid) {
        let root = Guid::new_random();
        let light = Guid::new_random();
        let knob = Guid::new_random();
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            crate::components::register_primitives(&mut registry);
            registry.register::<Transform>();
            registry.register::<Name>();
            registry.register::<Tags>();
            registry.register::<SmallVec<[Name; 4]>>();
            registry.register::<test_components::PointLight>();
            registry.register::<test_components::Vec3Adapter>();
        }
        world.init_resource::<PrefabLibrary>();
        (world, root, light, knob)
    }

    fn lamp_prefab(root: Guid, light: Guid, knob: Guid) -> String {
        format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Lamp", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
        }}),
        (guid: "{light}", name: "Light", parent: Some("{root}"), components: {{
            "regia_scene::prefab::tests::test_components::PointLight": (intensity: 100.0),
        }}),
        (guid: "{knob}", name: "Knob", parent: Some("{light}"), components: {{}}),
    ],
)
"#
        )
    }

    #[test]
    fn prefab_internal_guids_derive_from_the_root_guid() {
        let root_guid = Guid::from_u128(0xabcd);
        let mut seen = Vec::new();
        // One prefab file (its local ids) instantiated in two fresh worlds.
        let (_, root_id, light_id, knob_id) = registry_with();
        for _ in 0..2 {
            let (mut world, ..) = registry_with();
            let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
            let asset = Guid::from_u128(7);
            let registry = world.resource::<AppTypeRegistry>().0.clone();
            let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
            world.resource_mut::<PrefabLibrary>().insert(template);
            let instance_root = world.spawn_empty().insert(EntityGuid(root_guid)).id();
            let instance = PrefabInstanceData::plain(crate::format::SceneAssetRef(asset));
            let result =
                instantiate_prefab(&mut world, &instance, instance_root, &SpawnOptions::default(), 0)
                    .expect("instantiate");
            let mut guids: Vec<(Guid, Guid)> = result
                .locals
                .iter()
                .map(|(local, entity)| (*local, world.get::<EntityGuid>(*entity).expect("guid").0))
                .collect();
            guids.sort();
            seen.push(guids);
        }
        assert_eq!(
            seen[0], seen[1],
            "same root guid must give the same internal guids"
        );
        assert_ne!(
            derived_guid(root_guid, 1),
            derived_guid(Guid::from_u128(0xabce), 1)
        );
        assert_ne!(derived_guid(root_guid, 1), derived_guid(root_guid, 2));
    }

    /// `EntityGuid` defaults to nil. Two instances on nil roots used to derive the same member
    /// guids. `spawn_prefab` already refuses nil; `instantiate_prefab` must too.
    #[test]
    fn nil_root_guid_is_refused_before_spawn() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::from_u128(7);
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        let root = world.spawn(EntityGuid::default()).id();
        assert!(world.get::<EntityGuid>(root).is_some_and(|g| g.0.is_nil()));
        let err = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(asset)),
            root,
            &SpawnOptions::default(),
            0,
        )
        .expect_err("nil root guid");
        assert!(matches!(err, SceneSpawnError::Invalid(_)), "{err:?}");
        assert!(
            world.get::<Children>(root).is_none_or(|c| c.is_empty()),
            "nothing spawned"
        );
    }

    #[test]
    fn bake_and_instantiate_clones_components() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        assert!(template.warnings.is_empty(), "{:?}", template.warnings);
        world.resource_mut::<PrefabLibrary>().insert(template);

        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(asset)),
            instance_root,
            &SpawnOptions::default(),
            0,
        )
        .expect("instantiate");

        assert_eq!(result.entities_spawned, 3);
        let light = result.locals.get(&light_id).expect("light spawned");
        let light_value = world
            .get::<test_components::PointLight>(*light)
            .expect("light component");
        assert_eq!(light_value.intensity, 100.0);

        // Hierarchy: lamp root under instance root, knob under light.
        assert_eq!(
            world
                .get::<ChildOf>(result.locals[&root_id])
                .expect("child of root")
                .parent(),
            instance_root
        );
        assert_eq!(
            world.get::<ChildOf>(*light).expect("child of light").parent(),
            result.locals[&root_id]
        );
        assert!(world.get::<PrefabLink>(*light).is_some());
        assert!(world.get::<PrefabInstance>(instance_root).is_some());
    }

    /// A prefab entity named only by its record's `name` (what the editor's New Prefab writes, no
    /// `Name` component) keeps that name on every instance, as the scene spawner does for levels.
    /// An explicit `Name` component still wins, and an instance override of `Name` still applies.
    #[test]
    fn instances_keep_record_names_and_explicit_name_components_win() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root_id}", name: "Lamp", components: {{
            "regia_scene::transform::Transform": (),
        }}),
        (guid: "{light_id}", name: "Light", parent: Some("{root_id}"), components: {{
            "regia_scene::components::Name": "Bulb",
        }}),
        (guid: "{knob_id}", parent: Some("{root_id}"), components: {{}}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        assert!(template.warnings.is_empty(), "{:?}", template.warnings);
        world.resource_mut::<PrefabLibrary>().insert(template);

        let name_of =
            |world: &World, entity: Entity| world.get::<Name>(entity).map(|n| n.as_str().to_owned());
        for n in 1u128..=2 {
            // Instantiate refuses a root that has no EntityGuid (or a nil one). The name asserts
            // below stay on the spawned records; the root itself still has no Name.
            let instance_root = world.spawn(EntityGuid(Guid::from_u128(n))).id();
            let result = instantiate_prefab(
                &mut world,
                &PrefabInstanceData::plain(crate::format::SceneAssetRef(asset)),
                instance_root,
                &SpawnOptions::default(),
                0,
            )
            .expect("instantiate");
            assert_eq!(name_of(&world, result.locals[&root_id]).as_deref(), Some("Lamp"));
            assert_eq!(name_of(&world, result.locals[&light_id]).as_deref(), Some("Bulb"));
            assert_eq!(name_of(&world, result.locals[&knob_id]), None);
            assert_eq!(
                name_of(&world, instance_root),
                None,
                "the instance root keeps its own name"
            );
        }

        // An instance override of the name still applies over the record name.
        let instance_root = world.spawn(EntityGuid(Guid::from_u128(3))).id();
        let mut instance = PrefabInstanceData::plain(crate::format::SceneAssetRef(asset));
        instance.overrides.push(PrefabOverride {
            target: root_id,
            component: "regia_scene::components::Name".into(),
            path: String::new(),
            value: ron::value::RawValue::from_ron("\"Desk Lamp\"")
                .expect("raw")
                .to_owned(),
        });
        let result = instantiate_prefab(&mut world, &instance, instance_root, &SpawnOptions::default(), 0)
            .expect("instantiate with override");
        assert_eq!(
            name_of(&world, result.locals[&root_id]).as_deref(),
            Some("Desk Lamp")
        );
    }

    #[test]
    fn overrides_patch_fields_whole_components_and_unregistered_targets() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);

        let instance = PrefabInstanceData {
            prefab: crate::format::SceneAssetRef(asset),
            overrides: vec![
                // Field patch.
                crate::format::PrefabOverride {
                    target: light_id,
                    component: "regia_scene::prefab::tests::test_components::PointLight".into(),
                    path: ".intensity".into(),
                    value: ron::value::RawValue::from_ron("800.0").expect("ron").to_owned(),
                },
                // Whole-component replacement.
                crate::format::PrefabOverride {
                    target: knob_id,
                    component: "regia_scene::components::Tags".into(),
                    path: ".".into(),
                    value: ron::value::RawValue::from_ron(r#"(["bonus"])"#)
                        .expect("ron")
                        .to_owned(),
                },
                // Target that does not exist in the prefab.
                crate::format::PrefabOverride {
                    target: Guid::new_random(),
                    component: "regia_scene::components::Tags".into(),
                    path: ".0".into(),
                    value: ron::value::RawValue::from_ron(r#"(["x"])"#)
                        .expect("ron")
                        .to_owned(),
                },
            ],
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };

        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, instance_root, &SpawnOptions::default(), 0)
            .expect("instantiate");
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);

        let light = result.locals[&light_id];
        assert_eq!(
            world
                .get::<test_components::PointLight>(light)
                .expect("light")
                .intensity,
            800.0
        );
        let knob = result.locals[&knob_id];
        assert!(world.get::<Tags>(knob).is_some_and(|t| t.contains("bonus")));
    }

    #[test]
    fn removed_children_prune_subtrees() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);

        let instance = PrefabInstanceData {
            prefab: crate::format::SceneAssetRef(asset),
            removed_children: vec![light_id], // takes the knob with it
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };
        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, instance_root, &SpawnOptions::default(), 0)
            .expect("instantiate");
        assert_eq!(result.entities_spawned, 1);
        assert!(!result.locals.contains_key(&light_id));
        assert!(!result.locals.contains_key(&knob_id));
    }

    /// Registers a lamp prefab and an outer "room" prefab that nests it twice; returns the world,
    /// the outer asset and the lamp's light local id.
    fn room_with_two_lamps() -> (World, crate::format::SceneAssetRef, Guid) {
        let (mut world, inner_root, inner_light, inner_knob) = registry_with();
        let inner_file =
            SceneFile::from_ron_str(&lamp_prefab(inner_root, inner_light, inner_knob)).expect("parse");
        let inner_asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let inner_template =
            PrefabTemplate::from_scene_file(inner_asset, &inner_file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(inner_template);
        let (room, first, second) = (Guid::new_random(), Guid::new_random(), Guid::new_random());
        // A prefab has exactly one root: the room, with the two lamps under it.
        let outer_file = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{room}", name: "Room", components: {{}}),
        (guid: "{first}", name: "LampA", parent: Some("{room}"), components: {{}},
         prefab_instance: (prefab: "asset:{inner_asset}")),
        (guid: "{second}", name: "LampB", parent: Some("{room}"), components: {{}},
         prefab_instance: (prefab: "asset:{inner_asset}")),
    ],
)
"#
        ))
        .expect("parse");
        let outer_asset = Guid::new_random();
        let outer_template =
            PrefabTemplate::from_scene_file(outer_asset, &outer_file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(outer_template);
        (world, crate::format::SceneAssetRef(outer_asset), inner_light)
    }

    fn point_light_intensities(world: &mut World) -> Vec<f32> {
        world
            .query::<&test_components::PointLight>()
            .iter(world)
            .map(|light| light.intensity)
            .collect()
    }

    #[test]
    fn outer_override_applies_to_every_copy_of_a_nested_prefab() {
        let (mut world, outer, light_id) = room_with_two_lamps();
        let instance = PrefabInstanceData {
            overrides: vec![PrefabOverride {
                target: light_id,
                component: "regia_scene::prefab::tests::test_components::PointLight".to_owned(),
                path: ".intensity".to_owned(),
                value: ron::value::RawValue::from_ron("7.0").expect("raw").to_owned(),
            }],
            ..PrefabInstanceData::plain(outer)
        };
        let root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, root, &SpawnOptions::default(), 0)
            .expect("instantiate");
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(point_light_intensities(&mut world), vec![7.0, 7.0]);
    }

    #[test]
    fn removed_children_prunes_entities_inside_nested_prefabs() {
        let (mut world, outer, light_id) = room_with_two_lamps();
        let instance = PrefabInstanceData {
            removed_children: vec![light_id],
            ..PrefabInstanceData::plain(outer)
        };
        let root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, root, &SpawnOptions::default(), 0)
            .expect("instantiate");
        assert!(point_light_intensities(&mut world).is_empty());
        assert!(!result.locals.contains_key(&light_id));
        // The room root and its two lamp entities, each with a nested lamp root; the two lights
        // and two knobs are gone.
        assert_eq!(result.entities_spawned, 5);
    }

    #[test]
    fn removed_components_report_an_unknown_type_or_target() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        let missing_target = Guid::new_random();
        let instance = PrefabInstanceData {
            removed_components: vec![
                PrefabComponentRemove {
                    target: light_id,
                    component: "nope::Mesh".to_owned(),
                },
                PrefabComponentRemove {
                    target: missing_target,
                    component: "regia_scene::prefab::tests::test_components::PointLight".to_owned(),
                },
            ],
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };
        let root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, root, &SpawnOptions::default(), 0)
            .expect("instantiate");
        assert!(
            result.warnings.iter().any(|w| w.contains("nope::Mesh")),
            "{:?}",
            result.warnings
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains(&missing_target.to_string())),
            "{:?}",
            result.warnings
        );
        assert_eq!(point_light_intensities(&mut world), vec![100.0]);
    }

    #[test]
    fn nested_prefabs_compose_outer_wins() {
        // Inner: a lamp (reuse). Outer: a room containing the lamp with an override.
        let (mut world, inner_root, inner_light, inner_knob) = registry_with();
        let inner_file =
            SceneFile::from_ron_str(&lamp_prefab(inner_root, inner_light, inner_knob)).expect("parse");
        let inner_asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let inner_template =
            PrefabTemplate::from_scene_file(inner_asset, &inner_file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(inner_template);

        // Outer prefab: one entity that instantiates the inner one, overriding the light.
        let room_root = Guid::new_random();
        let outer_file = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{room_root}", name: "Room", components: {{
            "regia_scene::transform::Transform": (translation: (10.0, 0.0, 0.0)),
        }},
         prefab_instance: (
            prefab: "asset:{inner_asset}",
            overrides: [
                (target: "{inner_light}",
                 component: "regia_scene::prefab::tests::test_components::PointLight",
                 path: ".intensity",
                 value: 42.0),
            ],
         )),
    ],
)
"#
        ))
        .expect("parse");
        let outer_asset = Guid::new_random();
        let outer_template =
            PrefabTemplate::from_scene_file(outer_asset, &outer_file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(outer_template);

        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(outer_asset)),
            instance_root,
            &SpawnOptions::default(),
            0,
        )
        .expect("instantiate");

        // The nested lamp's light carries the outer override.
        let inner_light_entity = result.locals.get(&inner_light).expect("inner light spawned");
        let light = world
            .get::<test_components::PointLight>(*inner_light_entity)
            .expect("light");
        assert_eq!(light.intensity, 42.0);
        // ...and the outer world transform still composes down the chain.
        propagate_transforms(&mut world);
        let light_entity = *inner_light_entity;
        let room_entity = result.locals.get(&room_root).copied();
        let _ = room_entity;
        let light_world = world
            .get::<GlobalTransform>(light_entity)
            .expect("global")
            .translation();
        assert!(
            (light_world - Vec3::new(11.0, 0.0, 0.0)).length() < 1e-4,
            "room (10,0,0) + lamp (1,0,0) = {light_world:?}"
        );
    }

    #[test]
    fn instantiate_missing_template_reports_not_loaded() {
        let mut world = World::new();
        world.init_resource::<PrefabLibrary>();
        let root = world.spawn(EntityGuid(Guid::new_random())).id();
        let missing = Guid::new_random();
        let err = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(missing)),
            root,
            &SpawnOptions::default(),
            0,
        )
        .expect_err("missing prefab must fail");
        assert!(matches!(err, SceneSpawnError::PrefabNotLoaded(g) if g == missing));
    }

    #[test]
    fn a_failed_nested_instantiation_leaves_no_partial_instance_behind() {
        // A prefab that nests itself passes the missing-template precheck (every template is
        // loaded) and fails only at the nesting limit, after 64 levels were spawned.
        let (mut world, _, _, _) = registry_with();
        let asset = Guid::new_random();
        let entity = Guid::new_random();
        let file = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{entity}", name: "Loop", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#
        ))
        .expect("parse");
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let err = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(asset)),
            instance_root,
            &SpawnOptions::default(),
            0,
        )
        .expect_err("self-nesting prefab");
        assert!(matches!(err, SceneSpawnError::PrefabNestingTooDeep(_)), "{err:?}");
        assert!(
            world.get::<Children>(instance_root).is_none_or(|c| c.is_empty()),
            "the levels spawned before the failure are despawned"
        );
        assert!(world.get::<PrefabInstance>(instance_root).is_none());
        assert_eq!(world.query::<&PrefabLink>().iter(&world).count(), 0);
    }

    #[test]
    fn a_scene_spawn_that_fails_in_a_prefab_leaves_no_rows_behind() {
        let (mut world, _, _, _) = registry_with();
        let asset = Guid::new_random();
        let file = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Loop", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#,
            Guid::new_random()
        ))
        .expect("parse");
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        let scene = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{}", name: "Plain", components: {{}}),
        (guid: "{}", name: "Holder", components: {{}},
         prefab_instance: (prefab: "asset:{asset}", overrides: [])),
    ],
)
"#,
            Guid::new_random(),
            Guid::new_random()
        ))
        .expect("parse");
        let err = crate::spawn::spawn_scene(&mut world, &scene, &SpawnOptions::default())
            .expect_err("self-nesting prefab");
        assert!(matches!(err, SceneSpawnError::PrefabNestingTooDeep(_)), "{err:?}");
        assert_eq!(
            world.query::<&EntityGuid>().iter(&world).count(),
            0,
            "rows are despawned"
        );
    }

    #[test]
    fn subtree_size_terminates_on_a_child_cycle() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn(ChildOf(a)).id();
        world.entity_mut(a).insert(ChildOf(b));
        assert_eq!(subtree_size(&world, a), 2);
    }

    /// Correctness lane ecs-scene: the "known missing" set of `resolve_pending_prefabs` is reset
    /// by a library revision change. A fresh library that replaces the resource and reaches the
    /// same revision count (one insert each) must still count as a change, or the pending
    /// instance waits forever for a template that is already there.
    #[test]
    fn pending_prefabs_resolve_after_the_library_is_replaced() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let bake =
            |asset: Guid| PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        let wanted = Guid::from_u128(0x1a);
        let pending_root = world
            .spawn((
                EntityGuid(Guid::from_u128(0x2b)),
                PendingPrefab {
                    prefab: wanted.as_u128(),
                    ..Default::default()
                },
            ))
            .id();
        // The old library holds one unrelated template: the wanted one is recorded as missing.
        let mut old = PrefabLibrary::new();
        old.insert(bake(Guid::from_u128(0x3c)));
        world.insert_resource(old);
        resolve_pending_prefabs(&mut world);
        assert!(world.get::<PendingPrefab>(pending_root).is_some());

        // A fresh library replaces it, with one insert: the wanted template.
        let mut fresh = PrefabLibrary::new();
        fresh.insert(bake(wanted));
        world.insert_resource(fresh);
        resolve_pending_prefabs(&mut world);
        assert!(
            world.get::<PendingPrefab>(pending_root).is_none(),
            "the template is in the new library; the instance must resolve"
        );
        assert!(world.get::<PrefabInstance>(pending_root).is_some());
    }

    #[test]
    fn pending_prefabs_resolve_when_template_arrives() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let level = crate::level::LevelId(Guid::new_random());
        let pending_root = world
            .spawn((
                EntityGuid(Guid::new_random()),
                PendingPrefab {
                    prefab: asset.as_u128(),
                    root_transform: Some(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0))),
                    level: level.0.as_u128(),
                    persistent: false,
                    record: String::new(),
                },
            ))
            .id();
        world.init_resource::<PrefabLibrary>();

        // Not yet loaded: no children.
        resolve_pending_prefabs(&mut world);
        assert!(world.get::<PrefabLink>(pending_root).is_none());

        // Template arrives: the next run fills the instance.
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        resolve_pending_prefabs(&mut world);

        assert!(world.get::<PendingPrefab>(pending_root).is_none());
        assert!(world.get::<PrefabInstance>(pending_root).is_some());
        let mut children = 0;
        if let Some(kids) = world.get::<Children>(pending_root) {
            children = kids.len();
        }
        assert_eq!(children, 1, "lamp root attached under the pending root");
        assert_eq!(
            world.get::<Transform>(pending_root).map(|t| t.translation),
            Some(Vec3::new(5.0, 0.0, 0.0))
        );
        // Light entity present with its baked intensity.
        let found = world
            .query::<(&PrefabLink, &test_components::PointLight)>()
            .iter(&world)
            .any(|(link, _)| link.guid() == light_id);
        assert!(found);
        // The recorded spawn context carried through: instance root and spawned subtree both
        // carry the pending entity's level membership (unload_level must find them).
        assert_eq!(
            world.get::<LevelMember>(pending_root).map(|m| m.level()),
            Some(level)
        );
        let light_entity = world
            .query::<(Entity, &PrefabLink)>()
            .iter(&world)
            .find(|(_, link)| link.guid() == light_id)
            .map(|(e, _)| e)
            .expect("light");
        assert_eq!(
            world.get::<LevelMember>(light_entity).map(|m| m.level()),
            Some(level)
        );
    }

    #[test]
    fn added_children_and_components_apply() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);

        let badge = Guid::new_random();
        let instance = PrefabInstanceData {
            prefab: crate::format::SceneAssetRef(asset),
            added_components: vec![crate::format::PrefabComponentAdd {
                target: knob_id,
                component: "regia_scene::components::Tags".into(),
                value: ron::value::RawValue::from_ron(r#"(["chromed"])"#)
                    .expect("ron")
                    .to_owned(),
            }],
            added_children: vec![crate::format::SceneEntity {
                guid: badge,
                name: Some("Badge".into()),
                parent: Some(light_id),
                components: {
                    let mut map = std::collections::BTreeMap::new();
                    map.insert(
                        "regia_scene::transform::Transform".to_owned(),
                        ron::value::RawValue::from_ron("(translation: (0.0, 1.0, 0.0))")
                            .expect("ron")
                            .to_owned(),
                    );
                    map
                },
                prefab_instance: None,
            }],
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };

        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, instance_root, &SpawnOptions::default(), 0)
            .expect("instantiate");

        // Added component on the knob.
        let knob = result.locals[&knob_id];
        assert!(world.get::<Tags>(knob).is_some_and(|t| t.contains("chromed")));
        // Added child parented under the light entity.
        let badge_entity = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == badge)
            .map(|(e, _)| e)
            .expect("badge spawned");
        let light = result.locals[&light_id];
        assert_eq!(
            world.get::<ChildOf>(badge_entity).expect("badge parent").parent(),
            light
        );
    }

    /// Serializes a `Shape` whose `format_version` is one above `SHAPE_FORMAT_VERSION`, as a
    /// newer build would have written it.
    fn newer_shape_ron(world: &World) -> String {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let registry = handle.read();
        let value = crate::shape::Shape {
            format_version: crate::shape::SHAPE_FORMAT_VERSION + 1,
            ..Default::default()
        };
        let mut codec = ReflectCodec::new(&registry);
        codec.to_ron_typed(&value).expect("serialize shape")
    }

    fn register_shape(world: &World) {
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        registry.register::<crate::shape::Shape>();
    }

    /// Audit fix lane (scene correctness, finding 3): one added child's component RON that fails
    /// to decode downgrades to a warning, exactly like a top-level entity's component: the
    /// spawn succeeds, the instance root and the added child stay, and only that one component
    /// is missing. The top-level case is asserted in the same test so the two paths stay
    /// aligned.
    #[test]
    fn a_bad_component_ron_on_an_added_child_warns_and_keeps_the_scene() {
        let (mut world, root_id, light_id, _knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, _knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);

        let bad_transform = || {
            let mut map = std::collections::BTreeMap::new();
            map.insert(
                "regia_scene::transform::Transform".to_owned(),
                ron::value::RawValue::from_ron("(translation: \"nope\")")
                    .expect("ron")
                    .to_owned(),
            );
            map
        };
        let expect_transform_warning = |warnings: &[String]| {
            assert!(
                warnings.iter().any(|w| w.to_lowercase().contains("transform")),
                "{warnings:?}"
            );
        };
        fn find_guid(world: &mut World, guid: Guid) -> Option<Entity> {
            world
                .query::<(Entity, &crate::components::EntityGuid)>()
                .iter(world)
                .find(|(_, g)| g.0 == guid)
                .map(|(e, _)| e)
        }

        let holder = Guid::new_random();
        let badge = Guid::new_random();
        let mut scene = SceneFile::new();
        scene.entities.push(crate::format::SceneEntity {
            guid: holder,
            prefab_instance: Some(PrefabInstanceData {
                added_children: vec![crate::format::SceneEntity {
                    guid: badge,
                    name: Some("Badge".into()),
                    components: bad_transform(),
                    prefab_instance: None,
                    ..Default::default()
                }],
                ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
            }),
            ..Default::default()
        });

        let spawned = crate::spawn::spawn_scene(&mut world, &scene, &SpawnOptions::default())
            .expect("a bad added-child component RON is a warning, not a spawn failure");
        expect_transform_warning(&spawned.warnings);
        let holder_entity = find_guid(&mut world, holder).expect("instance root stays");
        assert!(world.get_entity(holder_entity).is_ok());
        let badge_entity = find_guid(&mut world, badge).expect("added child stays");
        assert!(
            world.get::<crate::transform::Transform>(badge_entity).is_none(),
            "the undecodable component is skipped, the rest of the child stays"
        );

        // The same component map on a top-level entity behaves the same way.
        let lone = Guid::new_random();
        let mut top = SceneFile::new();
        top.entities.push(crate::format::SceneEntity {
            guid: lone,
            components: bad_transform(),
            ..Default::default()
        });
        let spawned_top = crate::spawn::spawn_scene(&mut world, &top, &SpawnOptions::default())
            .expect("a bad top-level component RON is a warning");
        expect_transform_warning(&spawned_top.warnings);
        let lone_entity = find_guid(&mut world, lone).expect("top-level entity stays");
        assert!(world.get::<crate::transform::Transform>(lone_entity).is_none());
    }

    /// Audit fix lane (scene correctness, finding 2): the prefab bake path applies
    /// `FormatVersion` too. A template entity whose component is newer than
    /// `FormatVersion::CURRENT` is kept as raw text with a warning (no live value), so
    /// instantiation inserts nothing.
    #[test]
    fn bake_keeps_a_newer_format_version_component_as_raw_text() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        register_shape(&world);
        let shape_ron = newer_shape_ron(&world);
        let file = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root_id}", name: "Lamp", components: {{}}),
        (guid: "{light_id}", name: "Light", parent: Some("{root_id}"), components: {{
            "regia_scene::prefab::tests::test_components::PointLight": (intensity: 100.0),
            "regia_scene::Shape": {shape_ron},
        }}),
        (guid: "{knob_id}", name: "Knob", parent: Some("{light_id}"), components: {{}}),
    ],
)
"#
        ))
        .expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        assert!(
            template.warnings.iter().any(|w| w.contains("format_version")),
            "{:?}",
            template.warnings
        );
        world.resource_mut::<PrefabLibrary>().insert(template);

        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(asset)),
            instance_root,
            &SpawnOptions::default(),
            0,
        )
        .expect("instantiate");
        let light = result.locals.get(&light_id).expect("light spawned");
        assert!(
            world.get::<crate::shape::Shape>(*light).is_none(),
            "the newer-version component is not inserted"
        );
        assert!(
            world.get::<test_components::PointLight>(*light).is_some(),
            "the rest of the entity is intact"
        );
    }

    /// Audit fix lane (scene correctness, finding 2): the version check also covers instance
    /// edits: an `added_components` entry (a whole-component override) and an added child
    /// carrying a newer-version component are skipped with a warning.
    #[test]
    fn instance_edits_with_a_newer_format_version_component_warn_and_skip() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        register_shape(&world);
        let shape_ron = newer_shape_ron(&world);
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);

        let shape_raw = ron::value::RawValue::from_ron(&shape_ron)
            .expect("ron")
            .to_owned();
        let badge = Guid::new_random();
        let mut child_components = std::collections::BTreeMap::new();
        child_components.insert("regia_scene::Shape".to_owned(), shape_raw.clone());
        let instance = PrefabInstanceData {
            added_components: vec![crate::format::PrefabComponentAdd {
                target: knob_id,
                component: "regia_scene::Shape".into(),
                value: shape_raw,
            }],
            added_children: vec![crate::format::SceneEntity {
                guid: badge,
                components: child_components,
                ..Default::default()
            }],
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };
        let instance_root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, instance_root, &SpawnOptions::default(), 0)
            .expect("instantiate");
        let knob = result.locals.get(&knob_id).expect("knob spawned");
        assert!(
            world.get::<crate::shape::Shape>(*knob).is_none(),
            "the added component with a newer version is skipped"
        );
        let badge_entity = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == badge)
            .map(|(e, _)| e)
            .expect("added child spawned");
        assert!(
            world.get::<crate::shape::Shape>(badge_entity).is_none(),
            "the added child's newer-version component is skipped"
        );
        assert!(
            result.warnings.iter().any(|w| w.contains("format_version")),
            "{:?}",
            result.warnings
        );
    }

    #[test]
    fn a_scene_entity_parents_to_an_added_child_of_an_instance_in_either_file_order() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        let badge = Guid::new_random();
        let holder = Guid::new_random();
        let rider = Guid::new_random();
        let mut scene = SceneFile::new();
        scene.entities.push(crate::format::SceneEntity {
            guid: holder,
            prefab_instance: Some(PrefabInstanceData {
                added_children: vec![crate::format::SceneEntity {
                    guid: badge,
                    parent: Some(light_id),
                    ..Default::default()
                }],
                ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
            }),
            ..Default::default()
        });
        scene.entities.push(crate::format::SceneEntity {
            guid: rider,
            parent: Some(badge),
            ..Default::default()
        });
        scene.validate().expect("valid with the rider after the instance");
        crate::spawn::spawn_scene(&mut world, &scene, &SpawnOptions::default()).expect("spawn");
        let mut query = world.query::<(Entity, &crate::components::EntityGuid)>();
        let mut find = |world: &mut World, guid: Guid| {
            query
                .iter(world)
                .find(|(_, g)| g.0 == guid)
                .map(|(e, _)| e)
                .expect("spawned")
        };
        let (badge_e, rider_e) = (find(&mut world, badge), find(&mut world, rider));
        assert_eq!(world.get::<ChildOf>(rider_e).map(|c| c.parent()), Some(badge_e));
    }

    /// Bakes a room prefab whose one entity nests `inner_asset`; returns the room's asset id.
    fn bake_room_nesting(world: &mut World, inner_asset: Guid) -> Guid {
        let room_root = Guid::new_random();
        let outer_file = SceneFile::from_ron_str(&format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{room_root}", name: "Room", components: {{
            "regia_scene::transform::Transform": (translation: (10.0, 0.0, 0.0)),
        }},
         prefab_instance: (prefab: "asset:{inner_asset}")),
    ],
)
"#
        ))
        .expect("parse");
        let outer_asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template =
            PrefabTemplate::from_scene_file(outer_asset, &outer_file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);
        outer_asset
    }

    fn prefab_link_count(world: &mut World) -> usize {
        world.query::<&PrefabLink>().iter(world).count()
    }

    #[test]
    fn missing_nested_template_spawns_nothing_and_pends_on_the_outer_prefab() {
        let (mut world, inner_root, inner_light, inner_knob) = registry_with();
        let inner_asset = Guid::new_random();
        let outer_asset = bake_room_nesting(&mut world, inner_asset);

        // The outer template is loaded but its nested lamp is not: nothing may spawn.
        let probe = world.spawn_empty().id();
        let err = instantiate_prefab(
            &mut world,
            &PrefabInstanceData::plain(crate::format::SceneAssetRef(outer_asset)),
            probe,
            &SpawnOptions::default(),
            0,
        )
        .expect_err("inner missing");
        assert!(matches!(err, SceneSpawnError::PrefabNotLoaded(g) if g == inner_asset));
        assert_eq!(prefab_link_count(&mut world), 0, "no half-built outer subtree");

        // spawn_prefab leaves a pending marker naming the outer prefab.
        let root = world
            .spawn_prefab(outer_asset, None, &SpawnOptions::default(), Guid::new_random())
            .expect("pending");
        let pending = world.get::<PendingPrefab>(root).expect("pending marker");
        assert_eq!(pending.prefab, outer_asset.as_u128());
        assert_eq!(prefab_link_count(&mut world), 0);

        // Still not resolvable while the inner template is missing.
        resolve_pending_prefabs(&mut world);
        assert!(world.get::<PendingPrefab>(root).is_some());
        assert_eq!(prefab_link_count(&mut world), 0);

        // The inner template arrives: exactly one room entity plus the lamp subtree.
        let inner_file =
            SceneFile::from_ron_str(&lamp_prefab(inner_root, inner_light, inner_knob)).expect("parse");
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let inner_template =
            PrefabTemplate::from_scene_file(inner_asset, &inner_file, &registry.read()).expect("bake");
        let inner_len = inner_template.entities.len();
        world.resource_mut::<PrefabLibrary>().insert(inner_template);
        resolve_pending_prefabs(&mut world);
        assert!(world.get::<PendingPrefab>(root).is_none());
        // Room entity (1) + the inner template's entities; the room carries the nested root.
        assert_eq!(prefab_link_count(&mut world), 1 + inner_len);
    }

    fn text_component(raw: &str) -> std::collections::BTreeMap<String, Box<ron::value::RawValue>> {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            "regia_scene::transform::Transform".to_owned(),
            ron::value::RawValue::from_ron(raw).expect("ron").to_owned(),
        );
        map
    }

    /// A component RON on an added child that fails to decode skips just that component (the
    /// audit-mandated alignment with `spawn_components`): the spawn succeeds with a warning, the
    /// good child is intact, and the bad child stays without the component. Nothing can leak,
    /// because nothing is despawned mid-instantiation anymore; the spent-work-budget failure is
    /// still hard (`MAX_INSTANTIATION_WORK`).
    #[test]
    fn a_failed_added_child_component_warns_and_keeps_both_children() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        world.resource_mut::<PrefabLibrary>().insert(template);

        let good = Guid::new_random();
        let bad = Guid::new_random();
        let record = |guid, raw: &str| crate::format::SceneEntity {
            guid,
            name: None,
            parent: None,
            components: text_component(raw),
            prefab_instance: None,
        };
        let instance = PrefabInstanceData {
            added_children: vec![
                record(good, "(translation: (0.0, 1.0, 0.0))"),
                record(bad, "(translation: \"not a vector\")"),
            ],
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };
        let root = world.spawn(EntityGuid(Guid::new_random())).id();
        let result = instantiate_prefab(&mut world, &instance, root, &SpawnOptions::default(), 0)
            .expect("a bad component value is a warning, not a failure");
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("failed to load") && w.contains(&bad.to_string())),
            "{:?}",
            result.warnings
        );
        let find = |world: &mut World, guid: Guid| {
            world
                .query::<(Entity, &crate::components::EntityGuid)>()
                .iter(world)
                .find(|(_, g)| g.0 == guid)
                .map(|(e, _)| e)
        };
        let good_entity = find(&mut world, good).expect("good child stays");
        assert_eq!(
            world.get::<Transform>(good_entity).map(|t| t.translation),
            Some(Vec3::new(0.0, 1.0, 0.0)),
            "the good child keeps its transform"
        );
        let bad_entity = find(&mut world, bad).expect("bad child stays");
        assert!(
            world.get::<Transform>(bad_entity).is_none(),
            "only the undecodable component is skipped"
        );
        // Both children hang under the instance root; the template entities are intact.
        assert_eq!(world.get::<ChildOf>(good_entity).map(ChildOf::parent), Some(root));
        assert_eq!(world.get::<ChildOf>(bad_entity).map(ChildOf::parent), Some(root));
        assert_eq!(prefab_link_count(&mut world), 3, "lamp template entities present");
        assert!(
            world.get::<PrefabInstance>(root).is_some(),
            "the instance is recorded"
        );
    }

    #[test]
    fn added_child_with_unloaded_prefab_becomes_pending_and_resolves() {
        let (mut world, root_id, light_id, knob_id) = registry_with();
        let file = SceneFile::from_ron_str(&lamp_prefab(root_id, light_id, knob_id)).expect("parse");
        let asset = Guid::new_random();
        let registry = world.resource::<AppTypeRegistry>().0.clone();
        let template = PrefabTemplate::from_scene_file(asset, &file, &registry.read()).expect("bake");
        let template_len = template.entities.len();
        world.resource_mut::<PrefabLibrary>().insert(template);

        let late_asset = Guid::new_random();
        let badge = Guid::new_random();
        let instance = PrefabInstanceData {
            added_children: vec![crate::format::SceneEntity {
                guid: badge,
                name: None,
                parent: None,
                components: text_component("(translation: (0.0, 1.0, 0.0))"),
                prefab_instance: Some(PrefabInstanceData::plain(crate::format::SceneAssetRef(
                    late_asset,
                ))),
            }],
            ..PrefabInstanceData::plain(crate::format::SceneAssetRef(asset))
        };
        let root = world.spawn(EntityGuid(Guid::new_random())).id();
        instantiate_prefab(&mut world, &instance, root, &SpawnOptions::default(), 0).expect("instantiate");
        let badge_entity = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == badge)
            .map(|(e, _)| e)
            .expect("badge spawned");
        let pending = world.get::<PendingPrefab>(badge_entity).expect("pending marker");
        assert_eq!(pending.prefab, late_asset.as_u128());
        assert_eq!(prefab_link_count(&mut world), template_len);

        let late_file = SceneFile::from_ron_str(&lamp_prefab(
            Guid::new_random(),
            Guid::new_random(),
            Guid::new_random(),
        ))
        .expect("parse");
        let late_template =
            PrefabTemplate::from_scene_file(late_asset, &late_file, &registry.read()).expect("bake");
        let late_len = late_template.entities.len();
        world.resource_mut::<PrefabLibrary>().insert(late_template);
        resolve_pending_prefabs(&mut world);
        assert!(world.get::<PendingPrefab>(badge_entity).is_none());
        assert_eq!(prefab_link_count(&mut world), template_len + late_len);
    }
}
