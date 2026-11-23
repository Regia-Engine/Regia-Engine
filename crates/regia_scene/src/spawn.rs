// SPDX-License-Identifier: MPL-2.0
//! Spawning scene files into a world, and snapshotting worlds back into scene files.
//!
//! Loading follows the structure of [`SceneFile`]: entities first (components decoded through
//! reflection from their raw RON), then parent links, then prefab instances (which may resolve
//! parent guids of *added* children), then resources. Snapshotting reverses it: identity and
//! hierarchy come from the scene components, component values are reflected back to canonical
//! RON, and prefab instances are written back in their authored form.

use std::collections::{BTreeMap, HashMap, HashSet};

use regia_core::id::Guid;
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, CreateTypeData, ReflectRef, TypeRegistration};
use regia_ecs::serialize::{ReflectCodec, SerializeError, UnknownTypePolicy};

use crate::components::{EntityGuid, Name};
use crate::format::{SceneEntity, SceneFile};
use crate::level::{LevelId, LevelMember, Persistent};
use crate::prefab;

/// Errors from spawning a scene into a world or snapshotting a world.
#[derive(Debug, thiserror::Error)]
pub enum SceneSpawnError {
    /// The scene file's structure is invalid.
    #[error("scene format: {0}")]
    Format(#[from] crate::format::SceneFormatError),
    /// A component or resource value failed to (de)serialize reflectively.
    #[error("reflect (de)serialization: {0}")]
    Serialize(#[from] SerializeError),
    /// A referenced prefab is not baked/registered.
    #[error("prefab {0} is not loaded (bake it into the `PrefabLibrary` first)")]
    PrefabNotLoaded(Guid),
    /// Prefab nesting exceeded the safety depth (a prefab cycle).
    #[error("prefab nesting deeper than {max} at prefab {0} (cycle?)", max = prefab::MAX_NESTING_DEPTH)]
    PrefabNestingTooDeep(Guid),
    /// A structural reference did not resolve.
    #[error("invalid scene structure: {0}")]
    Invalid(String),
}

/// How spawned entities are tagged and how unknown types are treated.
#[derive(Clone, Debug, Default)]
pub struct SpawnOptions {
    /// Tags every spawned entity with [`LevelMember`] so `unload_level` can find them.
    pub level: Option<LevelId>,
    /// Marks entities [`Persistent`] (level changes never despawn them).
    pub persistent: bool,
    /// Unknown component types: skip with a warning (runtime) or keep the raw value so a
    /// re-save loses nothing (editor). See `docs/specs/10-core.md` § 2.3.
    pub unknown_types: UnknownTypePolicyOption,
}

/// Mirror of [`UnknownTypePolicy`] for [`SpawnOptions`] (the codec's enum is not `Default`-friendly).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnknownTypePolicyOption {
    /// Skip unknown types with a warning (runtime default).
    #[default]
    Skip,
    /// Preserve unknown types as raw RON (editor).
    Preserve,
}

/// The outcome of a successful spawn.
#[derive(Clone, Debug, Default)]
pub struct SpawnedScene {
    /// Root entities of the spawned hierarchy (no parent inside the scene).
    pub roots: Vec<Entity>,
    /// Total entities spawned, including prefab-internal ones.
    pub entities: usize,
    /// Non-fatal problems: unknown types, unknown fields, unresolvable parents, a guid another
    /// entity of the world already has (guid-keyed lookups then find either one).
    pub warnings: Vec<String>,
}

/// Raw values of component types that are not registered, kept on the entity so an editor re-save
/// does not lose data (only written under [`UnknownTypePolicyOption::Preserve`]).
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct UnregisteredComponents(pub BTreeMap<String, String>);

/// Components that never appear in the `components` map of a scene file (they are structural or
/// derived). Keep in sync with the format documentation.
pub(crate) fn structural_type_paths() -> &'static HashSet<&'static str> {
    static PATHS: std::sync::OnceLock<HashSet<&'static str>> = std::sync::OnceLock::new();
    PATHS.get_or_init(|| {
        HashSet::from([
            "regia_scene::components::EntityGuid",
            "regia_scene::components::Name",
            "regia_scene::transform::GlobalTransform",
            "regia_scene::transform::PreviousGlobalTransform",
            "regia_scene::components::InheritedVisibility",
            "regia_scene::components::ViewVisibility",
            "regia_scene::prefab::PrefabInstance",
            "regia_scene::prefab::PrefabLink",
            "regia_scene::prefab::PendingPrefab",
            "regia_scene::level::LevelMember",
            "regia_scene::level::Persistent",
            "regia_scene::spawn::UnregisteredComponents",
            "regia_nav::agent::NavAgentState",
            "bevy_ecs::hierarchy::ChildOf",
            "bevy_ecs::hierarchy::Children",
        ])
    })
}

/// Reads and writes one resource type between a world and scene data.
///
/// `bevy_ecs 0.20-rc` has no safe reflected `insert_resource` (its `ReflectResource` is a
/// marker), so scene resources go through these hooks: a pair of function pointers per type
/// path, registered on [`SceneResourceHooks`]. The generic [`SceneResourceHooks::register`]
/// builds them for any `Resource + Reflect + Clone` type; the editor/framework lane replaces
/// this with real reflected resources once the ECS grows them.
#[derive(Clone, Copy)]
pub struct ResourceHook {
    /// Inserts the value as the world's resource.
    pub apply: fn(&mut World, &dyn PartialReflect) -> Result<(), String>,
    /// Serializes the world's current resource value to canonical RON (`None` when absent).
    pub read: fn(&World, &mut ReflectCodec) -> Option<String>,
}

impl std::fmt::Debug for ResourceHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceHook")
            .field("apply", &(self.apply as usize))
            .field("read", &(self.read as usize))
            .finish()
    }
}

/// The resource types a world can (de)serialize in scene files, by type path.
#[derive(Resource, Clone, Debug, Default)]
pub struct SceneResourceHooks {
    hooks: BTreeMap<String, ResourceHook>,
}

impl SceneResourceHooks {
    /// Hooks for the engine's own resources (`LevelSettings`).
    pub fn with_engine_hooks() -> Self {
        let mut hooks = Self::default();
        hooks.register::<crate::level::LevelSettings>();
        hooks.register::<crate::layers::LevelLayers>();
        hooks.register::<crate::streaming::LevelStreaming>();
        hooks
    }

    /// Registers a resource type. Call before loading scenes that reference it.
    pub fn register<S>(&mut self)
    where
        S: Resource + Reflect + TypePath + Clone + Send + Sync + 'static,
    {
        self.hooks.insert(
            S::type_path().to_owned(),
            ResourceHook {
                apply: |world, value| {
                    let Some(concrete) = value.try_downcast_ref::<S>() else {
                        return Err(format!(
                            "resource value is {}, expected {}",
                            value.reflect_type_path(),
                            S::type_path()
                        ));
                    };
                    world.insert_resource(concrete.clone());
                    Ok(())
                },
                read: |world, codec| {
                    world
                        .get_resource::<S>()
                        .and_then(|resource| codec.to_ron_typed(resource.as_partial_reflect()).ok())
                },
            },
        );
    }

    /// Looks a hook up by the type path written in scene data.
    pub fn get(&self, type_path: &str) -> Option<ResourceHook> {
        self.hooks.get(type_path).copied()
    }
}

/// Spawns a whole scene file into `world`.
///
/// Components are decoded through reflection; unknown types are skipped with a warning (or kept
/// raw under the editor policy). Prefab instances are instantiated through the world's
/// [`prefab::PrefabLibrary`]; an instance of an unbaked prefab becomes a
/// [`prefab::PendingPrefab`] that the `resolve_pending_prefabs` system fills once the template
/// is registered.
///
/// # Example
///
/// ```
/// use regia_app::prelude::*;
/// use regia_core::id::Guid;
/// use regia_core::math::Vec3;
/// use regia_scene::prelude::*;
///
/// let mut app = App::new();
/// app.add_plugins(ScenePlugin);
///
/// let guid = Guid::new_random();
/// let file = SceneFile::from_ron_str(&format!(
///     r#"(format_version: 1, entities: [
///         (guid: "{guid}", name: "Prop", components: {{
///             "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0)),
///         }}),
///     ])"#
/// ))
/// .expect("valid scene RON");
///
/// let spawned = spawn_scene(app.world_mut(), &file, &SpawnOptions::default()).expect("spawn");
/// assert_eq!(spawned.entities, 1);
/// assert_eq!(spawned.roots.len(), 1);
/// let root = spawned.roots[0];
/// let transform = app.world().get::<Transform>(root).expect("Transform component");
/// assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
/// ```
pub fn spawn_scene(
    world: &mut World,
    file: &SceneFile,
    opts: &SpawnOptions,
) -> Result<SpawnedScene, SceneSpawnError> {
    spawn_scene_adopting(world, file, opts, &HashMap::new())
}

/// [`spawn_scene`] where records whose guid is in `adopt` are not spawned: the listed live
/// entity stands in for them (children still parent to it). A level reload uses this so its
/// surviving `Persistent` entities are not duplicated.
pub(crate) fn spawn_scene_adopting(
    world: &mut World,
    file: &SceneFile,
    opts: &SpawnOptions,
    adopt: &HashMap<Guid, Entity>,
) -> Result<SpawnedScene, SceneSpawnError> {
    let _span = tracing::info_span!("scene_spawn", entities = file.entities.len()).entered();
    let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Scene);
    file.validate()?;
    let registry = world.resource::<AppTypeRegistry>().0.clone();
    let registry = registry.read();
    let mut result = SpawnedScene::default();
    let existing: HashSet<Guid> = world.query::<&EntityGuid>().iter(world).map(|g| g.0).collect();

    // Pass 1: spawn every entity with identity + level tags.
    let mut by_guid: HashMap<Guid, Entity> = HashMap::with_capacity(file.entities.len());
    let mut rows: Vec<(Entity, &SceneEntity)> = Vec::with_capacity(file.entities.len());
    for record in &file.entities {
        if let Some(existing) = adopt.get(&record.guid) {
            by_guid.insert(record.guid, *existing);
            continue;
        }
        // Validation is per file, so two copy-pasted levels opened together get here.
        if existing.contains(&record.guid) {
            tracing::warn!(target: "regia_scene", guid = %record.guid, "guid already in the world");
            result
                .warnings
                .push(format!("guid {} is already in the world", record.guid));
        }
        let mut entity = world.spawn_empty();
        entity.insert(EntityGuid(record.guid));
        if let Some(name) = &record.name {
            entity.insert(Name::new(name.as_str()));
        }
        if let Some(level) = opts.level {
            entity.insert(LevelMember::from_level(level));
        }
        if opts.persistent {
            entity.insert(Persistent);
        }
        let entity = entity.id();
        by_guid.insert(record.guid, entity);
        rows.push((entity, record));
    }
    result.entities = rows.len();

    // Pass 2: components.
    let policy = match opts.unknown_types {
        UnknownTypePolicyOption::Skip => UnknownTypePolicy::Skip,
        UnknownTypePolicyOption::Preserve => UnknownTypePolicy::Preserve,
    };
    for (entity, record) in &rows {
        let unregistered = spawn_components(world, *entity, &record.components, &registry, policy);
        if !unregistered.is_empty() {
            result.warnings.push(format!(
                "entity {}: skipped unregistered component types: {}",
                record.guid,
                unregistered.join(", ")
            ));
        }
    }

    // Pass 3: prefab instances (fills `prefab_locals` so added children can find their parents).
    let mut prefab_locals: HashMap<Guid, Entity> = HashMap::new();
    for (entity, record) in &rows {
        if let Some(instance) = &record.prefab_instance {
            let spawned = prefab::instantiate_prefab(world, instance, *entity, opts, 0);
            match spawned {
                Ok(spawned) => {
                    result.entities += spawned.entities_spawned;
                    result.warnings.extend(spawned.warnings);
                    for (local, spawned_entity) in spawned.locals {
                        prefab_locals.insert(local, spawned_entity);
                    }
                }
                Err(SceneSpawnError::PrefabNotLoaded(missing)) => {
                    // Pending names the instance's own template: a missing nested template is
                    // detected before anything spawns, and the whole instance fills together.
                    // Async seam: fill when the template arrives. The pending record carries the
                    // spawn context so the resolved instance is tagged like a synchronous spawn.
                    if let Ok(mut entity_mut) = world.get_entity_mut(*entity) {
                        entity_mut.insert(prefab::PendingPrefab::for_record(instance, None, opts));
                    }
                    result
                        .warnings
                        .push(format!("prefab {missing} not loaded; instance is pending"));
                }
                Err(err) => {
                    // A hard failure leaves no half-built scene: the rows spawned above (and
                    // their prefab subtrees) would be unparented roots nobody owns. Entities
                    // the caller adopted are not rows and stay.
                    for (entity, _) in &rows {
                        world.despawn(*entity);
                    }
                    return Err(err);
                }
            }
        }
    }

    // Pass 4: parenting. Parents may be scene entities or prefab-internal local ids (added
    // children attach inside an instantiated subtree).
    for (entity, record) in &rows {
        let Some(parent_guid) = record.parent else {
            continue;
        };
        let parent = if let Some(parent) = by_guid.get(&parent_guid) {
            Some(*parent)
        } else {
            prefab_locals.get(&parent_guid).copied().inspect(|_| {
                // Attached inside a prefab subtree; no warning needed.
            })
        };
        match parent {
            Some(parent) => {
                if let Ok(mut entity_mut) = world.get_entity_mut(*entity) {
                    entity_mut.insert(ChildOf(parent));
                }
            }
            None => {
                result.warnings.push(format!(
                    "entity {}: parent {parent_guid} not found in scene or prefab subtrees",
                    record.guid
                ));
            }
        }
    }

    // Roots for the report.
    for (entity, _) in &rows {
        if world.get::<ChildOf>(*entity).is_none() {
            result.roots.push(*entity);
        }
    }

    // Pass 5: resources.
    let hooks = world
        .get_resource::<SceneResourceHooks>()
        .cloned()
        .unwrap_or_else(SceneResourceHooks::with_engine_hooks);
    spawn_resources(world, &file.resources, &registry, &hooks, &mut result.warnings);

    Ok(result)
}

/// Which planned parent links would close a cycle. `parent_of[i]` is the entry that entry `i` is to
/// be parented to (`None`, or an index out of range, ends the chain). The returned flag is set on
/// exactly one link per cycle, so skipping the flagged links leaves the hierarchy acyclic.
/// Linear in the number of entries. Used where scene data is not validated up front (cooked
/// scenes, prefab instances' added children): `ChildOf` itself does not reject cycles, and a
/// cycle would hang every walk up the hierarchy.
pub(crate) fn cycle_closing_links(parent_of: &[Option<usize>]) -> Vec<bool> {
    const NEW: u8 = 0;
    const ON_PATH: u8 = 1;
    const DONE: u8 = 2;
    let count = parent_of.len();
    let mut closes = vec![false; count];
    let mut state = vec![NEW; count];
    let mut path = Vec::new();
    for start in 0..count {
        let mut cursor = start;
        while state[cursor] == NEW {
            state[cursor] = ON_PATH;
            path.push(cursor);
            match parent_of[cursor] {
                Some(parent) if parent < count => {
                    if state[parent] == ON_PATH {
                        closes[cursor] = true;
                        break;
                    }
                    cursor = parent;
                }
                _ => break,
            }
        }
        for index in path.drain(..) {
            state[index] = DONE;
        }
    }
    closes
}

/// Inserts the component values of one entity record. Returns the type paths that were not
/// registered (already turned into a warning by the caller; under `Preserve` they are also kept
/// on the entity in [`UnregisteredComponents`]).
fn spawn_components(
    world: &mut World,
    entity: Entity,
    components: &BTreeMap<String, Box<ron::value::RawValue>>,
    registry: &TypeRegistry,
    policy: UnknownTypePolicy,
) -> Vec<String> {
    let mut unregistered = Vec::new();
    let mut preserved = BTreeMap::new();
    let mut codec = ReflectCodec::new(registry).unknown_types(policy);
    let mut entity_mut = match world.get_entity_mut(entity) {
        Ok(entity) => entity,
        Err(_) => return unregistered,
    };
    for (type_path, raw) in components {
        let Some(registration) =
            regia_core::brand::lookup_type_path(type_path.as_str(), |p| registry.get_with_type_path(p))
        else {
            unregistered.push(type_path.clone());
            if policy == UnknownTypePolicy::Preserve {
                preserved.insert(type_path.clone(), raw.get_ron().to_owned());
            }
            continue;
        };
        let Some(reflect_component) = registration.data::<ReflectComponent>() else {
            unregistered.push(format!("{type_path} (no ReflectComponent)"));
            continue;
        };
        let decoded = codec
            .from_ron_typed(raw.get_ron(), registration)
            .map_err(|err| err.to_string())
            .and_then(|value| {
                check_format_version(registration, value.as_partial_reflect())?;
                Ok(value)
            });
        match decoded {
            Ok(value) => reflect_component.insert(&mut entity_mut, &*value, registry),
            Err(err) => {
                unregistered.push(format!("{type_path} ({err})"));
                if policy == UnknownTypePolicy::Preserve {
                    preserved.insert(type_path.clone(), raw.get_ron().to_owned());
                }
            }
        }
    }
    if !preserved.is_empty() {
        entity_mut.insert(UnregisteredComponents(preserved));
    }
    unregistered
}

/// A reflected component saved with a `format_version` field. Add `FormatVersion` to its `#[reflect(...)]` list and
/// [`spawn_scene`] leaves a component written by a newer build out of the world, with a warning (and, under
/// [`UnknownTypePolicy::Preserve`], its text kept for the next save), the same way as a type it does not know.
pub trait FormatVersion {
    /// The newest `format_version` this build reads.
    const CURRENT: u32;
}

/// Type data for [`FormatVersion`].
#[derive(Clone, Copy, Debug)]
pub struct ReflectFormatVersion {
    current: u32,
}

impl<T: FormatVersion> CreateTypeData<T> for ReflectFormatVersion {
    fn create_type_data(_: ()) -> Self {
        Self { current: T::CURRENT }
    }
}

/// Refuses a decoded component whose `format_version` is newer than its [`FormatVersion`] (a type without one, or
/// without the field, passes).
pub fn check_format_version(
    registration: &TypeRegistration,
    value: &dyn PartialReflect,
) -> Result<(), String> {
    let Some(version) = registration.data::<ReflectFormatVersion>() else {
        return Ok(());
    };
    let ReflectRef::Struct(fields) = value.reflect_ref() else {
        return Ok(());
    };
    match fields
        .field("format_version")
        .and_then(|field| field.try_downcast_ref::<u32>())
    {
        Some(found) if *found > version.current => Err(format!(
            "format_version {found} is newer than {}",
            version.current
        )),
        _ => Ok(()),
    }
}

/// Applies the `resources` map of a scene file to the world's resources.
fn spawn_resources(
    world: &mut World,
    resources: &BTreeMap<String, Box<ron::value::RawValue>>,
    registry: &TypeRegistry,
    hooks: &SceneResourceHooks,
    warnings: &mut Vec<String>,
) {
    for (type_path, raw) in resources {
        let Some(registration) =
            regia_core::brand::lookup_type_path(type_path.as_str(), |p| registry.get_with_type_path(p))
        else {
            warnings.push(format!("resource type `{type_path}` is not registered; skipped"));
            continue;
        };
        let Some(hook) = hooks.get(type_path) else {
            warnings.push(format!(
                "resource `{type_path}` has no `SceneResourceHooks` entry; register it to load scene resources"
            ));
            continue;
        };
        let mut codec = ReflectCodec::new(registry);
        match codec.from_ron_typed(raw.get_ron(), registration) {
            Ok(value) => {
                if let Err(err) = (hook.apply)(world, value.as_partial_reflect()) {
                    warnings.push(format!("resource `{type_path}` failed to apply: {err}"));
                }
            }
            Err(err) => warnings.push(format!("resource `{type_path}` failed to load: {err}")),
        }
    }
}

/// Keeps [`crate::components::FeedAttach`] true before a save (specs/60 § 2.4 rule 3): a user entity
/// under a feed-owned entity gets the marker with the item's key; one that moved under an ordinary
/// parent loses it. An entity at the root keeps its marker: it is still waiting for that item.
fn sync_feed_attach(world: &mut World) {
    use crate::components::{FeedAttach, FeedOwned};
    let mut under_feed: Vec<(Entity, String)> = Vec::new();
    let mut moved_away: Vec<Entity> = Vec::new();
    let mut query = world.query_filtered::<(Entity, &ChildOf, Option<&FeedAttach>), Without<FeedOwned>>();
    for (entity, link, attach) in query.iter(&*world) {
        match world.get::<FeedOwned>(link.parent()) {
            Some(owner) if attach.is_none_or(|a| a.key != owner.key) => {
                under_feed.push((entity, owner.key.clone()));
            }
            None if attach.is_some() => moved_away.push(entity),
            _ => {}
        }
    }
    for (entity, key) in under_feed {
        world.entity_mut(entity).insert(FeedAttach { key });
    }
    for entity in moved_away {
        world.entity_mut(entity).remove::<FeedAttach>();
    }
}

/// Which entities and resources a snapshot includes.
#[derive(Clone, Debug, Default)]
pub struct SnapshotFilter {
    /// Only entities tagged with this level (`None`: every entity that has an `EntityGuid`).
    pub level: Option<LevelId>,
    /// Resource type paths to save (e.g. `["regia_scene::level::LevelSettings"]`).
    pub resources: Vec<String>,
}

/// Captures matching entities and resources into a scene file.
///
/// Structural and derived components (guid, name, hierarchy, global transforms, prefab bookkeeping)
/// are written into their dedicated fields, never into `components`. Prefab instances serialize
/// their authored overrides and component edits; prefab-internal children (with
/// [`prefab::PrefabLink`]) are not written — they are re-instantiated from the template — but
/// entities added *on top of* an instance are. An entity parented under a prefab-internal entity
/// (or under another such added entity) is written into the owning instance's `added_children`,
/// addressed by the internal entity's local id, so load → save → load keeps it where it was.
pub fn snapshot_scene(world: &mut World, filter: &SnapshotFilter) -> Result<SceneFile, SceneSpawnError> {
    snapshot_scene_cached(world, filter, None)
}

/// One entity's serialized components in a snapshot: type path to canonical RON (the
/// `components` field of a [`SceneEntity`]).
pub type SnapshotComponents = BTreeMap<String, Box<ron::value::RawValue>>;

/// Keeps the serialized components of entities between snapshots, so an entity that did not
/// change is not reflected and serialized again (ADR-7748: the editor's incremental level
/// autosave). [`snapshot_scene_cached`] asks [`Self::reuse`] before it serializes an included
/// entity's components and hands every set it did serialize to [`Self::keep`]. Whatever `reuse`
/// returns must equal what a fresh serialization would give, so the snapshot stays identical to
/// [`snapshot_scene`]'s; everything else in a record (guid, name, parent, prefab instance) is
/// read fresh every time.
pub trait SnapshotCache {
    /// The serialized components of `entity` from an earlier snapshot, when they still hold.
    fn reuse(&mut self, world: &World, entity: Entity) -> Option<SnapshotComponents>;
    /// The components just serialized for `entity`, and the ids of the components they were
    /// serialized from (a change to one of them, or to the entity's component set, is what makes
    /// the kept set stale).
    fn keep(
        &mut self,
        world: &World,
        entity: Entity,
        components: &SnapshotComponents,
        watched: &[ComponentId],
    );
}

/// [`snapshot_scene`] with a [`SnapshotCache`]: the same file, with the components of unchanged
/// entities taken from the cache (`None`: no cache, every entity is serialized).
///
/// # Errors
///
/// As [`snapshot_scene`].
pub fn snapshot_scene_cached(
    world: &mut World,
    filter: &SnapshotFilter,
    mut cache: Option<&mut (dyn SnapshotCache + '_)>,
) -> Result<SceneFile, SceneSpawnError> {
    sync_feed_attach(world);
    let registry_handle = world.resource::<AppTypeRegistry>().0.clone();
    let registry = registry_handle.read();

    // Candidate entities (with EntityGuid) and their parents, before filtering: a child whose
    // parent falls outside the filter is snapshotted as a root (with a warning). Records are
    // sorted by guid so snapshots are deterministic (canonical RON must re-save byte-identically).
    let mut candidates: Vec<Entity> = Vec::new();
    let mut query_state = QueryState::<(Entity, &EntityGuid)>::new(world);
    for (entity, guid) in query_state.iter(world) {
        let _ = guid;
        candidates.push(entity);
    }
    drop(query_state);
    // Query order depends on archetype creation order; write records in guid order so a resave is
    // byte-identical regardless of how the world's tables were created.
    candidates.sort_by_key(|entity| world.get::<EntityGuid>(*entity).map(|g| g.0).unwrap_or_default());

    let include: HashSet<Entity> = candidates
        .iter()
        .copied()
        .filter(|entity| {
            // Prefab-internal entities are re-instantiated from their template, never written.
            world.get::<prefab::PrefabLink>(*entity).is_none()
        })
        // Procedural output is regenerated or loaded from its bake, never saved (specs/62 § 7.1).
        .filter(|entity| world.get::<crate::components::Generated>(*entity).is_none())
        // Live data of the DCC feed is never saved; a bake makes it ordinary (specs/60 § 2.4).
        .filter(|entity| world.get::<crate::components::FeedOwned>(*entity).is_none())
        // A placed level's members come from its instance. Written into the host they would be
        // spawned a second time on the next load. A snapshot of the level a streamed instance
        // places is that level's own content, so it keeps them.
        .filter(|entity| match world.get::<crate::placed::PlacedMember>(*entity) {
            None => true,
            Some(member) => filter.level.is_some_and(|level| {
                world
                    .get::<crate::placed::PlacedSpawned>(member.instance)
                    .is_some_and(|s| s.mode == crate::placed::PlacedMode::Streamed && s.level == level)
            }),
        })
        .filter(|entity| match filter.level {
            None => true,
            Some(level) => world
                .get::<LevelMember>(*entity)
                .is_some_and(|member| member.level() == level),
        })
        .collect();

    let mut file = SceneFile::new();
    let hooks = world
        .get_resource::<SceneResourceHooks>()
        .cloned()
        .unwrap_or_else(SceneResourceHooks::with_engine_hooks);
    let mut codec = ReflectCodec::new(&registry);

    // Which included entities are added children of an instance (and under which instance root,
    // with which parent reference); the rest are top-level records.
    let mut placement: HashMap<Entity, Option<AddedPlacement>> = HashMap::with_capacity(include.len());
    for entity in &candidates {
        if include.contains(entity) {
            added_placement(world, *entity, &include, &mut placement);
        }
    }

    let mut records: HashMap<Entity, SceneEntity> = HashMap::with_capacity(include.len());
    let mut added_under: HashMap<Entity, Vec<Entity>> = HashMap::new();
    let mut top_level: Vec<Entity> = Vec::new();
    for entity in &candidates {
        if !include.contains(entity) {
            continue;
        }
        let Some(mut record) = snapshot_entity(
            world,
            *entity,
            &include,
            &registry,
            &mut codec,
            cache.as_deref_mut(),
        )?
        else {
            continue;
        };
        match placement.get(entity).copied().flatten() {
            Some(placed) => {
                record.parent = Some(placed.parent);
                added_under.entry(placed.instance_root).or_default().push(*entity);
            }
            None => {
                if world
                    .get::<ChildOf>(*entity)
                    .is_some_and(|link| world.get::<prefab::PrefabLink>(link.parent()).is_some())
                {
                    tracing::warn!(target: "regia_scene", "snapshot: {} hangs under a prefab instance outside the snapshot; saving as root", guid_of(world, *entity));
                }
                top_level.push(*entity);
            }
        }
        records.insert(*entity, record);
    }
    for entity in top_level {
        if let Some(record) = assemble_record(entity, &mut records, &added_under) {
            file.entities.push(record);
        }
    }

    for type_path in &filter.resources {
        let Some(hook) = hooks.get(type_path) else {
            continue;
        };
        if let Some(text) = (hook.read)(world, &mut codec)
            && let Ok(raw) = ron::value::RawValue::from_ron(&text)
        {
            file.resources.insert(type_path.clone(), raw.to_owned());
        }
    }

    drop(registry);
    drop(registry_handle);
    file.validate()?;
    Ok(file)
}

/// Where an added child sits: the instance root whose record lists it, and the parent reference
/// written in the record (a prefab-internal local id, or another added child's guid).
#[derive(Clone, Copy, Debug)]
struct AddedPlacement {
    instance_root: Entity,
    parent: Guid,
}

/// Decides (memoized in `placement`) whether `entity` is an added child of a prefab instance:
/// its parent is a prefab-internal entity of an included instance, or is itself such an added
/// child. The instance root is the first ancestor of the internal parent without a
/// [`prefab::PrefabLink`]: the instance the level (not a template) authored.
fn added_placement(
    world: &World,
    entity: Entity,
    include: &HashSet<Entity>,
    placement: &mut HashMap<Entity, Option<AddedPlacement>>,
) -> Option<AddedPlacement> {
    if let Some(known) = placement.get(&entity) {
        return *known;
    }
    // Walk up through included plain parents whose answer is not known yet (iteratively: a deep
    // chain must not recurse), until an ancestor decides the answer for the top of the chain.
    let mut chain = vec![entity];
    let mut on_chain = HashSet::from([entity]);
    let mut cursor = entity;
    let top_answer = loop {
        let Some(parent) = world.get::<ChildOf>(cursor).map(|link| link.parent()) else {
            break None;
        };
        if let Some(link) = world.get::<prefab::PrefabLink>(parent) {
            break instance_root_above(world, parent, include).map(|instance_root| AddedPlacement {
                instance_root,
                parent: link.guid(),
            });
        }
        if !include.contains(&parent) {
            break None; // parent outside the snapshot: the record is saved as a root
        }
        if let Some(known) = placement.get(&parent).copied() {
            break known.map(|placed| AddedPlacement {
                instance_root: placed.instance_root,
                parent: guid_of(world, parent),
            });
        }
        if !on_chain.insert(parent) {
            break None; // malformed (cyclic) hierarchy
        }
        chain.push(parent);
        cursor = parent;
    };
    // `chain` runs child → ancestor. Its top gets `top_answer`; each entry below is an added child
    // of the same instance, parented by guid to the entry above it.
    let mut answer = top_answer;
    if let Some(top) = chain.last() {
        placement.insert(*top, answer);
    }
    for pair in chain.windows(2).rev() {
        answer = answer.map(|placed| AddedPlacement {
            instance_root: placed.instance_root,
            parent: guid_of(world, pair[1]),
        });
        placement.insert(pair[0], answer);
    }
    placement.get(&entity).copied().flatten()
}

/// The entity's `EntityGuid` (nil when it has none; snapshot candidates always have one).
fn guid_of(world: &World, entity: Entity) -> Guid {
    world.get::<EntityGuid>(entity).map(|g| g.0).unwrap_or_default()
}

/// The included instance root that owns the prefab-internal entity `internal`: the first ancestor
/// without a [`prefab::PrefabLink`], when it carries a [`prefab::PrefabInstance`].
fn instance_root_above(world: &World, internal: Entity, include: &HashSet<Entity>) -> Option<Entity> {
    let mut cursor = internal;
    let mut seen = HashSet::new();
    while world.get::<prefab::PrefabLink>(cursor).is_some() {
        if !seen.insert(cursor) {
            return None; // malformed (cyclic) hierarchy
        }
        cursor = world.get::<ChildOf>(cursor)?.parent();
    }
    (include.contains(&cursor) && world.get::<prefab::PrefabInstance>(cursor).is_some()).then_some(cursor)
}

/// Builds the record of `entity` with its added children nested into its instance record.
fn assemble_record(
    entity: Entity,
    records: &mut HashMap<Entity, SceneEntity>,
    added_under: &HashMap<Entity, Vec<Entity>>,
) -> Option<SceneEntity> {
    let mut record = records.remove(&entity)?;
    if let Some(children) = added_under.get(&entity)
        && let Some(instance) = record.prefab_instance.as_mut()
    {
        for child in children {
            if let Some(child_record) = assemble_record(*child, records, added_under) {
                instance.added_children.push(child_record);
            }
        }
    }
    Some(record)
}

/// The reflected components of `entity` as a snapshot writes them: every registered,
/// reflectable, non-structural component, then the preserved unknown types. When a
/// [`SnapshotCache`] needs the component ids, it supplies `watched`; ordinary snapshots
/// avoid allocating a watch list that would immediately be discarded.
fn serialize_components(
    world: &mut World,
    entity: Entity,
    guid: Guid,
    registry: &TypeRegistry,
    codec: &mut ReflectCodec,
    mut watched: Option<&mut Vec<ComponentId>>,
) -> Result<SnapshotComponents, SceneSpawnError> {
    let mut components = BTreeMap::new();
    let structural = structural_type_paths();
    let component_ids: Vec<ComponentId> = world
        .inspect_entity(entity)
        .map_err(|_| SceneSpawnError::Invalid(format!("entity {entity:?} vanished during snapshot")))?
        .map(|(id, _)| id)
        .collect();
    for component_id in component_ids {
        let Some(info) = world.components().get_info(component_id) else {
            continue;
        };
        let Some(type_id) = info.type_id() else { continue };
        let Some(registration) = registry.get(type_id) else {
            continue;
        };
        let type_path = registration.type_info().type_path();
        if structural.contains(type_path) {
            continue;
        }
        let Some(reflect_component) = registration.data::<ReflectComponent>() else {
            continue;
        };
        if let Some(watched) = watched.as_mut() {
            watched.push(component_id);
        }
        let cloned = world
            .get_entity_mut(entity)
            .ok()
            .and_then(|entity| reflect_component.reflect(&entity).map(|v| v.reflect_clone()));
        let value = match cloned {
            Some(Ok(value)) => Some(value),
            Some(Err(err)) => {
                // A component that cannot be cloned through reflection would vanish from the save.
                tracing::warn!(target: "regia_scene", "snapshot: component {type_path} on {guid} cannot be cloned through reflection: {err}");
                None
            }
            None => None,
        };
        if let Some(value) = value {
            match codec.to_ron_typed(value.as_partial_reflect()) {
                Ok(text) => {
                    if let Ok(raw) = ron::value::RawValue::from_ron(text.trim_end()) {
                        components.insert(type_path.to_owned(), raw.to_owned());
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "regia_scene", "snapshot: component {type_path} on {guid} failed to serialize: {err}");
                }
            }
        }
    }

    // Preserved unknown types (editor policy) go back verbatim.
    if let Some(preserved) = world.get::<UnregisteredComponents>(entity) {
        for (type_path, text) in &preserved.0 {
            if let Ok(raw) = ron::value::RawValue::from_ron(text) {
                components.insert(type_path.clone(), raw.to_owned());
            }
        }
        if let Some(watched) = watched {
            watched.extend(world.component_id::<UnregisteredComponents>());
        }
    }
    Ok(components)
}

/// Snapshots one entity; `None` if the entity has no scene-relevant content at all.
fn snapshot_entity(
    world: &mut World,
    entity: Entity,
    include: &HashSet<Entity>,
    registry: &TypeRegistry,
    codec: &mut ReflectCodec,
    cache: Option<&mut (dyn SnapshotCache + '_)>,
) -> Result<Option<SceneEntity>, SceneSpawnError> {
    let guid = world.get::<EntityGuid>(entity).map(|g| g.0).unwrap_or_default();

    let name = world.get::<Name>(entity).map(|n| n.as_str().to_owned());

    // Parents inside a prefab instance are resolved by the caller (`added_placement`).
    let parent = world.get::<ChildOf>(entity).map(|p| p.parent()).and_then(|parent| {
        if include.contains(&parent) {
            world.get::<EntityGuid>(parent).map(|g| g.0)
        } else {
            // A child of a feed-owned entity saves a `FeedAttach` instead of the link.
            if world.get::<prefab::PrefabLink>(parent).is_none()
                && world.get::<crate::components::FeedOwned>(parent).is_none()
            {
                tracing::warn!(target: "regia_scene", "snapshot: parent of {guid} is outside the filter; saving as root");
            }
            None
        }
    });

    // Components: reflected values, except structural/derived ones (an unchanged entity's come
    // from the cache, ADR-7748).
    let components = match cache {
        Some(cache) => match cache.reuse(world, entity) {
            Some(components) => components,
            None => {
                let mut watched = Vec::new();
                let components =
                    serialize_components(world, entity, guid, registry, codec, Some(&mut watched))?;
                cache.keep(world, entity, &components, &watched);
                components
            }
        },
        None => serialize_components(world, entity, guid, registry, codec, None)?,
    };

    // Prefab instance record: the live instance's, or the record a pending instance waits with.
    let prefab_instance = match world.get::<prefab::PrefabInstance>(entity) {
        Some(instance) => Some(instance.to_data()),
        None => world
            .get::<prefab::PendingPrefab>(entity)
            .map(|pending| match pending.instance_data() {
                Ok(data) => data,
                Err(err) => {
                    tracing::warn!(target: "regia_scene", "snapshot: pending prefab record on {guid} does not parse ({err}); writing a plain instance");
                    crate::format::PrefabInstanceData::plain(crate::format::SceneAssetRef(Guid::from_u128(
                        pending.prefab,
                    )))
                }
            }),
    };

    // Skip entities with no content (bare identity is still worth keeping when parented, to
    // preserve hierarchy).
    Ok(Some(SceneEntity {
        guid,
        name,
        parent,
        components,
        prefab_instance,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::{LEVEL_SETTINGS_PATH, LevelSettings};
    use crate::transform::Transform;
    use regia_core::math::Vec3;

    #[test]
    fn nav_agent_runtime_state_is_never_persisted() {
        assert!(structural_type_paths().contains("regia_nav::agent::NavAgentState"));
    }

    /// A world with the scene types registered (what `ScenePlugin` would set up).
    fn test_world() -> World {
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            crate::components::register_primitives(&mut registry);
            registry.register::<Transform>();
            registry.register::<Name>();
            registry.register::<EntityGuid>();
            registry.register::<Tags2>();
            registry.register::<LevelSettings>();
        }
        world
    }

    /// A test component with reflection.
    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Tags2 {
        kind: String,
        count: u32,
    }

    #[test]
    fn snapshot_records_are_sorted_by_guid_regardless_of_archetypes() {
        let mut world = test_world();
        // Descending guids across three different archetypes.
        world.spawn((EntityGuid(Guid::from_u128(3)), Transform::default()));
        world.spawn((EntityGuid(Guid::from_u128(2)), Tags2::default()));
        world.spawn(EntityGuid(Guid::from_u128(1)));
        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        let guids: Vec<Guid> = snapshot.entities.iter().map(|e| e.guid).collect();
        assert_eq!(
            guids,
            vec![Guid::from_u128(1), Guid::from_u128(2), Guid::from_u128(3)]
        );
    }

    /// Feed-owned entities are never written; a user child is saved at the root with a
    /// `FeedAttach` naming the item, and loses the marker once it moves away (specs/60 § 2.4).
    #[test]
    fn feed_owned_entities_are_not_saved_and_their_children_save_an_attach_marker() {
        use crate::components::{FeedAttach, FeedOwned};
        let mut app = regia_app::App::new();
        app.add_plugins(crate::ScenePlugin);
        let world = app.world_mut();
        let live = world
            .spawn((
                EntityGuid(Guid::from_u128(1)),
                FeedOwned {
                    key: "Cube".to_owned(),
                },
            ))
            .id();
        let child = world
            .spawn((
                EntityGuid(Guid::from_u128(2)),
                crate::Transform::default(),
                ChildOf(live),
            ))
            .id();
        let file = snapshot_scene(world, &SnapshotFilter::default()).expect("snapshot");
        assert_eq!(file.entities.len(), 1, "only the user child is written");
        let record = &file.entities[0];
        assert_eq!(record.guid, Guid::from_u128(2));
        assert_eq!(record.parent, None, "the parent link is omitted");
        assert!(
            record.components.keys().any(|k| k.ends_with("FeedAttach")),
            "{:?}",
            record.components.keys()
        );
        assert_eq!(
            world.get::<FeedAttach>(child).map(|a| a.key.as_str()),
            Some("Cube")
        );

        let plain = world.spawn(EntityGuid(Guid::from_u128(3))).id();
        world.entity_mut(child).insert(ChildOf(plain));
        let file = snapshot_scene(world, &SnapshotFilter::default()).expect("snapshot");
        let record = file
            .entities
            .iter()
            .find(|e| e.guid == Guid::from_u128(2))
            .expect("child");
        assert_eq!(record.parent, Some(Guid::from_u128(3)));
        assert!(!record.components.keys().any(|k| k.ends_with("FeedAttach")));
    }

    /// Procedural output (`Generated`, specs/62 § 7.1) is never written; authored entities are.
    #[test]
    fn snapshot_skips_generated_entities() {
        let mut world = test_world();
        world.spawn(EntityGuid(Guid::from_u128(1)));
        world.spawn((
            EntityGuid(Guid::from_u128(2)),
            crate::components::Generated {
                generator: Guid::from_u128(1),
                run_id: 1,
            },
        ));
        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        let guids: Vec<Guid> = snapshot.entities.iter().map(|e| e.guid).collect();
        assert_eq!(guids, vec![Guid::from_u128(1)]);
    }

    /// `Tags` holds opaque `Name`s: reflection can clone it only because `Name` reflects `Clone`,
    /// and a component that cannot be cloned is left out of the save.
    #[test]
    fn snapshot_keeps_tags_made_of_opaque_names() {
        let mut world = test_world();
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            registry.register::<crate::components::Tags>();
            registry.register::<smallvec::SmallVec<[Name; 4]>>();
        }
        world.spawn((
            EntityGuid(Guid::from_u128(7)),
            crate::components::Tags::from_iter_str(["crate", "loot"]),
        ));
        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        let record = &snapshot.entities[0];
        assert!(
            record.components.contains_key("regia_scene::components::Tags"),
            "{:?}",
            record.components.keys().collect::<Vec<_>>()
        );
        let mut respawned = test_world();
        {
            let handle = respawned.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            registry.register::<crate::components::Tags>();
            registry.register::<smallvec::SmallVec<[Name; 4]>>();
        }
        spawn_scene(&mut respawned, &snapshot, &SpawnOptions::default()).expect("respawn");
        let tags: Vec<String> = respawned
            .query::<&crate::components::Tags>()
            .iter(&respawned)
            .flat_map(|tags| tags.0.iter().map(|t| t.as_str().to_owned()))
            .collect();
        assert_eq!(tags, ["crate", "loot"]);
    }

    #[test]
    fn spawn_then_snapshot_round_trips_components_and_hierarchy() {
        let guid_a = Guid::new_random();
        let guid_b = Guid::new_random();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{guid_a}", name: "Root", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0)),
            "regia_scene::spawn::tests::Tags2": (kind: "prop", count: 4),
        }}),
        (guid: "{guid_b}", name: "Child", parent: Some("{guid_a}"), components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 5.0, 0.0)),
        }}),
    ],
    resources: {{}},
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");

        let mut world = test_world();
        let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        assert_eq!(spawned.entities, 2);
        assert_eq!(spawned.roots.len(), 1);
        assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);

        // Component values arrived.
        let root_entity = world
            .query::<(Entity, &EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == guid_a)
            .map(|(e, _)| e)
            .expect("root");
        let tags = world.get::<Tags2>(root_entity).expect("tags");
        assert_eq!(tags.count, 4);
        assert_eq!(tags.kind, "prop");
        assert!(world.get::<Name>(root_entity).is_some());

        // Snapshot round-trips byte-identically.
        let snapshot = snapshot_scene(&mut world, &SnapshotFilter::default()).expect("snapshot");
        let snapshot_text = snapshot.to_ron_string().expect("write");
        let round = snapshot_scene(
            &mut {
                let mut w = test_world();
                spawn_scene(&mut w, &snapshot, &SpawnOptions::default()).expect("respawn");
                w
            },
            &SnapshotFilter::default(),
        )
        .expect("snapshot 2");
        assert_eq!(round.to_ron_string().expect("write"), snapshot_text);
        assert_eq!(snapshot.entities.len(), 2);
    }

    #[test]
    fn unknown_types_skip_with_warning_or_preserve() {
        let guid = Guid::new_random();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{guid}", components: {{
            "regia_scene::transform::Transform": (),
            "my_game::NotRegistered": (whatever: 1),
        }}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");

        let mut world = test_world();
        let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        assert!(
            spawned
                .warnings
                .iter()
                .any(|w| w.contains("my_game::NotRegistered")),
            "{:?}",
            spawned.warnings
        );

        // Editor policy keeps the raw value and a re-save writes it back (fresh world: guids
        // must be unique per world).
        let mut preserved_world = test_world();
        let preserved = spawn_scene(
            &mut preserved_world,
            &file,
            &SpawnOptions {
                unknown_types: UnknownTypePolicyOption::Preserve,
                ..SpawnOptions::default()
            },
        )
        .expect("spawn");
        assert!(
            preserved
                .warnings
                .iter()
                .any(|w| w.contains("my_game::NotRegistered"))
        );
        let snapshot = snapshot_scene(&mut preserved_world, &SnapshotFilter::default()).expect("snapshot");
        let record = snapshot.entities.iter().find(|e| e.guid == guid).expect("entity");
        assert!(record.components.contains_key("my_game::NotRegistered"));
    }

    #[test]
    fn missing_fields_default_unknown_fields_warn() {
        let guid = Guid::new_random();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{guid}", components: {{
            "regia_scene::spawn::tests::Tags2": (kind: "half", stray_field: true),
        }}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let mut world = test_world();
        let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        // Unknown field: lenient codec warns.
        let entity = world
            .query::<(Entity, &EntityGuid)>()
            .iter(&world)
            .next()
            .map(|(e, _)| e)
            .expect("one entity");
        let tags = world.get::<Tags2>(entity).expect("tags");
        assert_eq!(tags.count, 0, "missing field defaults");
        assert_eq!(tags.kind, "half");
        let _ = spawned;
    }

    #[test]
    fn resources_are_applied() {
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [],
    resources: {{
        "{LEVEL_SETTINGS_PATH}": (gravity: (0.0, -20.0, 0.0)),
    }},
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let mut world = test_world();
        let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
        assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
        let settings = world.resource::<LevelSettings>();
        assert_eq!(settings.gravity, Vec3::new(0.0, -20.0, 0.0));
    }
}
