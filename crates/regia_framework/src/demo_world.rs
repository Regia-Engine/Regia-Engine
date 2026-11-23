// SPDX-License-Identifier: MPL-2.0
//! ECS-wide demo recording, playback and seeking (`book/agents/demo.md`, ADR-1878): the
//! [`DemoSim`] seam of [`crate::demo`] implemented for a whole world, so a game records and scrubs
//! a demo without writing its own snapshot code.
//!
//! # What is tracked
//!
//! The simulation state of a demo is every entity carrying [`DemoTracked`] (all of its reflected
//! components, minus the short type names in [`DemoWorldConfig::excluded_components`]) plus every
//! resource registered with
//! [`App::register_demo_resource`](RegisterDemoResourceExt::register_demo_resource). By default
//! the derived presentation components `GlobalTransform` and `PreviousGlobalTransform` are
//! excluded (transform propagation runs in `PostUpdate`, which a seek's catch-up steps do not run),
//! and so is `Children`, the mirror of each child's `ChildOf`.
//!
//! - [`world_demo_hash`] is the keyframe hash: [`crate::state_hash`]'s canonical text of the
//!   tracked entities (keyed by `Name`, never by entity id) and resources (entity references in
//!   them print as those keys too), folded with FNV-1a.
//! - [`world_demo_snapshot`] is the keyframe snapshot: each tracked entity's components as typed
//!   postcard ([`ReflectCodec`]), plus the tracked resources, in entity-id order.
//! - [`restore_world_demo`] puts the world back to a snapshot. Tracked entities that still exist
//!   keep their ids and get their components replaced (components added since are removed);
//!   tracked entities spawned since are despawned; entities despawned since are spawned again (as
//!   new entities, never on a live untracked entity that happens to hold the same id), and
//!   entity references inside restored components and resources are remapped to the new ids through
//!   `ReflectMapEntities` (a type that holds an `Entity` must register it, as for scenes). Tracked
//!   resources added since are removed. Everything is decoded before the world is touched, so a
//!   snapshot that cannot be read changes nothing. [`DemoRemap`] then lists the entities that came
//!   back under new ids, for ids held outside the tracked state (the spectator follows through it).
//!
//! # Stepping a world
//!
//! [`WorldDemoSim`] runs one fixed step the way a frame does: it schedules the recorded action row
//! through [`InputInjection`] for the coming frame, then runs `First` (the clocks advance one
//! frame), `PreUpdate` (actions are evaluated, injected values included) and `FixedUpdate`. With
//! [`enable_fixed_step`] (one fixed step per frame, fixed frame time) that is exactly the
//! simulation part of [`App::update`], so a seek lands on the state a straight run reaches; a test
//! pins this for every tick. `Update`, `PostUpdate` and rendering do not run during catch-up, so a
//! game keeps its simulation in `FixedUpdate`, as determinism already requires. [`DemoCatchUp`] is
//! in the world while such a step runs: the agent dispatcher, which lives in `PreUpdate`, waits for
//! the end of the catch-up, and a game system that must not repeat (a sound, a hit marker) can
//! check it too.
//!
//! State outside the tracked set is not restored: the clocks ([`Time`]), the evaluated action
//! phases of the frame before the keyframe, untracked entities. A simulation that reads them
//! diverges after a seek, and the next keyframe check reports it (`diverged`) instead of showing
//! a wrong state.
//!
//! # The live driver
//!
//! [`DemoWorldPlugin`] adds [`DemoDriver`], which records, plays, pauses and seeks a running app.
//! Requests ([`DemoDriver::request`]) are applied by an exclusive system in `Last`, outside every
//! schedule a catch-up runs; the agent methods `demo.record_start`, `demo.record_stop`,
//! `demo.play`, `demo.pause`, `demo.seek` and `demo.status` go through it. Recording uses
//! [`InputRecorder`] for the input rows and timestamps and captures a keyframe whenever the step
//! count reaches the next interval. Playback replays through [`InputRecorder`] and checks every
//! keyframe hash it lands on; a mismatch pauses the replay and names the tick. Pausing sets
//! [`Time::paused`], so no fixed step runs while a viewer looks around.

use std::collections::BTreeSet;
use std::path::PathBuf;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::entity::{EntityHashMap, EntityHashSet, EntityMapper};
use regia_ecs::reflect::{
    AppTypeRegistry, ReflectComponent, ReflectFromReflect, ReflectMapEntities, ReflectResource, TypeRegistry,
};
use regia_ecs::serialize::ReflectCodec;
use regia_input::InputInjection;
use serde::{Deserialize, Serialize};

use crate::demo::{DemoError, DemoHeader, DemoKeyframe, DemoRecording, DemoSim, SeekReport};
use crate::input_recording::{InputRecorder, RecorderMode};
use crate::state_hash::{
    RESOURCE_KEY_PREFIX, canonical_text_with_entity_markers, replace_entities, snapshot_hash,
    world_snapshot_keyed,
};

/// The version of the snapshot bytes [`world_demo_snapshot`] writes (stored inside demo
/// keyframes).
pub const WORLD_SNAPSHOT_VERSION: u32 = 1;

/// Components left out of hashes and snapshots unless the game changes
/// [`DemoWorldConfig::excluded_components`]: the transforms derived in `PostUpdate` (a seek's
/// catch-up does not refresh them) and `Children`, which the ECS keeps in step with each child's
/// `ChildOf` (restoring `ChildOf` rebuilds it).
pub const DEFAULT_EXCLUDED_COMPONENTS: &[&str] = &["GlobalTransform", "PreviousGlobalTransform", "Children"];

/// Marks an entity as part of the demo simulation state (see the module docs).
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct DemoTracked;

/// What a world demo hashes and snapshots.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct DemoWorldConfig {
    /// Short component type names (`Transform`, `Health`) left out of keyframe hashes and
    /// snapshots: presentation state a replay may differ in, or state a seek cannot restore.
    pub excluded_components: Vec<String>,
}

impl Default for DemoWorldConfig {
    fn default() -> Self {
        Self {
            excluded_components: DEFAULT_EXCLUDED_COMPONENTS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

impl DemoWorldConfig {
    fn keeps(&self, short: &str) -> bool {
        short != "DemoTracked" && !self.excluded_components.iter().any(|name| name == short)
    }
}

/// In the world while [`step_world`] runs the schedules of one step (module docs, "Stepping a
/// world"). The agent dispatcher answers no request while it is present, so a client never sees
/// the world or the driver halfway through a seek.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DemoCatchUp;

/// Written by every [`restore_world_demo`]: the tracked entities it spawned again, as
/// `snapshot id -> new entity`. Code that holds an entity id outside the tracked state (the
/// spectator's followed entity, a UI selection) maps it through this after a seek.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct DemoRemap {
    /// Snapshot entity id -> the entity that now stands for it (entries whose id changed only).
    pub moved: EntityHashMap<Entity>,
}

impl DemoRemap {
    /// The entity that now stands for `entity` after the last restore, if it came back under a new
    /// id.
    #[must_use]
    pub fn get(&self, entity: Entity) -> Option<Entity> {
        self.moved.get(&entity).copied()
    }
}

type ResourceSerializeFn = fn(&World, &TypeRegistry) -> Option<Result<Vec<u8>, String>>;
type ResourceDecodeFn = fn(&TypeRegistry, &[u8]) -> Result<Box<dyn Reflect>, String>;
type ResourceApplyFn = fn(&mut World, &TypeRegistry, Box<dyn Reflect>, &mut dyn EntityMapper);
type ResourceRemoveFn = fn(&mut World);
type ResourceTextFn = fn(&World) -> Option<String>;

/// One tracked resource type and its type-erased snapshot functions.
#[derive(Debug, Clone, Copy)]
struct TrackedResource {
    type_path: &'static str,
    short_name: &'static str,
    serialize: ResourceSerializeFn,
    decode: ResourceDecodeFn,
    apply: ResourceApplyFn,
    remove: ResourceRemoveFn,
    text: ResourceTextFn,
}

/// The resources a world demo tracks (registered with
/// [`App::register_demo_resource`](RegisterDemoResourceExt::register_demo_resource)).
#[derive(Resource, Debug, Default, Clone)]
pub struct DemoTrackedResources {
    entries: Vec<TrackedResource>,
}

impl DemoTrackedResources {
    /// Tracks resource `T` (idempotent).
    pub fn register<T: Resource + Reflect + FromReflect + TypePath + GetTypeRegistration>(&mut self) {
        if self.entries.iter().any(|entry| entry.type_path == T::type_path()) {
            return;
        }
        self.entries.push(TrackedResource {
            type_path: T::type_path(),
            short_name: T::short_type_path(),
            serialize: |world, registry| {
                let resource = world.get_resource::<T>()?;
                let mut codec = ReflectCodec::new(registry);
                Some(
                    codec
                        .to_postcard_typed(resource.as_partial_reflect())
                        .map_err(|e| e.to_string()),
                )
            },
            decode: |registry, bytes| {
                let registration = registry
                    .get(std::any::TypeId::of::<T>())
                    .ok_or_else(|| format!("resource {} is not registered for reflection", T::type_path()))?;
                let mut codec = ReflectCodec::new(registry);
                let value = codec
                    .from_postcard_typed(bytes, registration)
                    .map_err(|e| format!("resource {}: {e}", T::type_path()))?;
                let typed = <T as FromReflect>::from_reflect(value.as_partial_reflect())
                    .ok_or_else(|| format!("snapshot of resource {} has the wrong type", T::type_path()))?;
                Ok(Box::new(typed))
            },
            apply: |world, registry, value, mapper| {
                let Ok(mut typed) = value.downcast::<T>() else {
                    return;
                };
                if let Some(map) = registry
                    .get(std::any::TypeId::of::<T>())
                    .and_then(|registration| registration.data::<ReflectMapEntities>())
                {
                    map.map_entities(typed.as_partial_reflect_mut(), mapper);
                }
                world.insert_resource(*typed);
            },
            remove: |world| {
                world.remove_resource::<T>();
            },
            text: |world| {
                world
                    .get_resource::<T>()
                    .map(|resource| canonical_text_with_entity_markers(resource.as_partial_reflect()))
            },
        });
    }

    /// The tracked resource type paths, in registration order.
    pub fn type_paths(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.type_path)
    }
}

/// [`App`] extension: [`App::register_demo_resource`](RegisterDemoResourceExt::register_demo_resource).
pub trait RegisterDemoResourceExt {
    /// Makes resource `T` part of the demo state: registers it for reflection and adds it to
    /// [`DemoTrackedResources`] (hashed and snapshotted at every keyframe, restored by a seek).
    fn register_demo_resource<T: Resource + Reflect + FromReflect + TypePath + GetTypeRegistration>(
        &mut self,
    ) -> &mut Self;
}

impl RegisterDemoResourceExt for App {
    fn register_demo_resource<T: Resource + Reflect + FromReflect + TypePath + GetTypeRegistration>(
        &mut self,
    ) -> &mut Self {
        self.register_type::<T>()
            .init_resource::<DemoTrackedResources>()
            .world_mut()
            .resource_mut::<DemoTrackedResources>()
            .register::<T>();
        self
    }
}

/// The world's demo state hash (module docs, "What is tracked").
#[must_use]
pub fn world_demo_hash(world: &World) -> u64 {
    let config = world
        .get_resource::<DemoWorldConfig>()
        .cloned()
        .unwrap_or_default();
    let (mut snapshot, keys) = world_snapshot_keyed(
        world,
        |entity| entity.contains::<DemoTracked>(),
        |short| config.keeps(short),
    );
    if let Some(tracked) = world.get_resource::<DemoTrackedResources>() {
        for entry in &tracked.entries {
            if let Some(text) = (entry.text)(world) {
                // Entity references print as entity keys, as in components: not as ids.
                snapshot.insert(
                    format!("{RESOURCE_KEY_PREFIX}{}", entry.short_name),
                    replace_entities(&text, &keys),
                );
            }
        }
    }
    snapshot_hash(&snapshot)
}

/// The postcard shape of a world snapshot.
#[derive(Serialize, Deserialize)]
struct WorldSnapshot {
    version: u32,
    entities: Vec<SnapshotEntity>,
    resources: Vec<SnapshotResource>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotEntity {
    /// `Entity::to_bits` when the snapshot was taken.
    id: u64,
    /// Full component type path -> typed postcard (two types may share a short name).
    components: Vec<(String, Vec<u8>)>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotResource {
    type_path: String,
    data: Vec<u8>,
}

fn registry_of(world: &World) -> AppTypeRegistry {
    world
        .get_resource::<AppTypeRegistry>()
        .cloned()
        .unwrap_or_default()
}

/// The tracked, kept, reflected components of `entity`: `(full type path, type id)`.
fn tracked_components(
    world: &World,
    registry: &TypeRegistry,
    config: &DemoWorldConfig,
    entity: Entity,
) -> Vec<(String, std::any::TypeId)> {
    let Ok(infos) = world.inspect_entity(entity) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (_, info) in infos {
        let Some(type_id) = info.type_id() else {
            continue;
        };
        let Some(registration) = registry.get(type_id) else {
            continue;
        };
        if registration.data::<ReflectResource>().is_some()
            || registration.data::<ReflectComponent>().is_none()
        {
            continue;
        }
        let paths = registration.type_info().type_path_table();
        if config.keeps(paths.short_path()) {
            out.push((paths.path().to_string(), type_id));
        }
    }
    out.sort();
    out
}

/// The world's demo snapshot (module docs, "What is tracked").
///
/// # Errors
/// [`DemoError::Invalid`] naming the first tracked component or resource that cannot be encoded
/// (exclude it with [`DemoWorldConfig::excluded_components`] or make it serializable).
pub fn world_demo_snapshot(world: &World) -> Result<Vec<u8>, DemoError> {
    let config = world
        .get_resource::<DemoWorldConfig>()
        .cloned()
        .unwrap_or_default();
    let registry = registry_of(world);
    let registry = registry.read();
    let mut codec = ReflectCodec::new(&registry);
    let mut ids: Vec<Entity> = world
        .iter_entities()
        .filter(|entity| entity.contains::<DemoTracked>())
        .map(|entity| entity.id())
        .collect();
    ids.sort_by_key(|entity| entity.to_bits());
    let mut entities = Vec::with_capacity(ids.len());
    for entity in ids {
        let mut components = Vec::new();
        for (path, type_id) in tracked_components(world, &registry, &config, entity) {
            let Ok(value) = world.get_reflect(entity, type_id) else {
                continue;
            };
            let bytes = codec.to_postcard_typed(value.as_partial_reflect()).map_err(|e| {
                DemoError::Invalid(format!(
                    "component {path} on entity {entity} cannot be snapshotted: {e}"
                ))
            })?;
            components.push((path, bytes));
        }
        entities.push(SnapshotEntity {
            id: entity.to_bits(),
            components,
        });
    }
    let mut resources = Vec::new();
    if let Some(tracked) = world.get_resource::<DemoTrackedResources>() {
        for entry in &tracked.entries {
            match (entry.serialize)(world, &registry) {
                None => {}
                Some(Ok(data)) => resources.push(SnapshotResource {
                    type_path: entry.type_path.to_string(),
                    data,
                }),
                Some(Err(e)) => {
                    return Err(DemoError::Invalid(format!(
                        "resource {} cannot be snapshotted: {e}",
                        entry.type_path
                    )));
                }
            }
        }
    }
    postcard::to_allocvec(&WorldSnapshot {
        version: WORLD_SNAPSHOT_VERSION,
        entities,
        resources,
    })
    .map_err(|e| DemoError::Invalid(e.to_string()))
}

/// Maps snapshot entity ids to live entities; ids outside the snapshot map to themselves.
struct SnapshotMapper<'a>(&'a EntityHashMap<Entity>);

impl EntityMapper for SnapshotMapper<'_> {
    fn get_mapped(&mut self, source: Entity) -> Entity {
        self.0.get(&source).copied().unwrap_or(source)
    }

    fn set_mapped(&mut self, _source: Entity, _target: Entity) {}
}

/// One snapshot entity's components, decoded: `(full type path, value)`.
type DecodedComponents = Vec<(String, Box<dyn PartialReflect>)>;

/// Puts the world back to a [`world_demo_snapshot`] (module docs, "What is tracked").
///
/// # Errors
/// A reason when the bytes are not a world snapshot of a version this build reads, or a component
/// or resource in it cannot be decoded with the current types (a different build). The world is
/// unchanged then.
#[tracing::instrument(skip_all, target = "regia_framework", name = "demo_world_restore")]
pub fn restore_world_demo(world: &mut World, bytes: &[u8]) -> Result<(), String> {
    let snapshot: WorldSnapshot =
        postcard::from_bytes(bytes).map_err(|e| format!("not a world snapshot: {e}"))?;
    if snapshot.version != WORLD_SNAPSHOT_VERSION {
        return Err(format!(
            "world snapshot version {} is not supported (this build reads {WORLD_SNAPSHOT_VERSION})",
            snapshot.version
        ));
    }
    let config = world
        .get_resource::<DemoWorldConfig>()
        .cloned()
        .unwrap_or_default();
    let registry = registry_of(world);
    let registry = registry.read();
    let mut codec = ReflectCodec::new(&registry);

    // Decode everything before touching the world, so a bad snapshot changes nothing.
    let mut decoded: Vec<(Entity, DecodedComponents)> = Vec::new();
    for saved in &snapshot.entities {
        let id = Entity::try_from_bits(saved.id).ok_or_else(|| format!("bad entity id {}", saved.id))?;
        let mut components = Vec::with_capacity(saved.components.len());
        for (path, data) in &saved.components {
            let registration = registry
                .get_with_type_path(path)
                .ok_or_else(|| format!("component {path} is not registered for reflection"))?;
            let value: Box<dyn PartialReflect> = codec
                .from_postcard_typed(data, registration)
                .map_err(|e| format!("component {path}: {e}"))?;
            components.push((path.clone(), value));
        }
        decoded.push((id, components));
    }
    let entries: Vec<TrackedResource> = world
        .get_resource::<DemoTrackedResources>()
        .map(|tracked| tracked.entries.clone())
        .unwrap_or_default();
    let mut resources = Vec::with_capacity(snapshot.resources.len());
    for saved in &snapshot.resources {
        let entry = entries
            .iter()
            .find(|entry| entry.type_path == saved.type_path)
            .ok_or_else(|| format!("resource {} is not tracked by this build", saved.type_path))?;
        resources.push((entry, (entry.decode)(&registry, &saved.data)?));
    }

    // Entities: keep the live ones, despawn the ones spawned since, respawn the ones gone since.
    let wanted: BTreeSet<u64> = snapshot.entities.iter().map(|e| e.id).collect();
    let live: Vec<Entity> = world
        .iter_entities()
        .filter(|entity| entity.contains::<DemoTracked>())
        .map(|entity| entity.id())
        .collect();
    let mut kept: EntityHashSet = EntityHashSet::default();
    for entity in live {
        if wanted.contains(&entity.to_bits()) {
            kept.insert(entity);
        } else if world.get_entity(entity).is_ok() {
            // Despawning a parent takes its children along: this one may be gone already.
            world.despawn(entity);
        }
    }
    // Only a live tracked entity keeps its id. Any other live entity with a snapshot id (an
    // untracked one, a resource's entity, one spawned since for another snapshot id) is not it.
    let mut map: EntityHashMap<Entity> = EntityHashMap::default();
    for (id, _) in &decoded {
        let target = if kept.contains(id) && world.get_entity(*id).is_ok() {
            *id
        } else {
            world.spawn_empty().id()
        };
        map.insert(*id, target);
    }

    for (id, components) in decoded {
        let target = map.get(&id).copied().unwrap_or(id);
        // Components the entity gained since the snapshot go.
        let names: BTreeSet<&str> = components.iter().map(|(path, _)| path.as_str()).collect();
        let extra: Vec<std::any::TypeId> = tracked_components(world, &registry, &config, target)
            .into_iter()
            .filter(|(path, _)| !names.contains(path.as_str()))
            .map(|(_, type_id)| type_id)
            .collect();
        let Ok(mut entity) = world.get_entity_mut(target) else {
            continue;
        };
        entity.insert(DemoTracked);
        for type_id in extra {
            if let Some(reflect) = registry
                .get(type_id)
                .and_then(|registration| registration.data::<ReflectComponent>())
            {
                reflect.remove(&mut entity);
            }
        }
        for (path, value) in components {
            let Some(registration) = registry.get_with_type_path(&path) else {
                continue;
            };
            let Some(reflect) = registration.data::<ReflectComponent>() else {
                continue;
            };
            // Entity references inside the value follow the respawned entities.
            let mut value = value;
            if let (Some(mapper), Some(from_reflect)) = (
                registration.data::<ReflectMapEntities>(),
                registration.data::<ReflectFromReflect>(),
            ) && let Some(concrete) = from_reflect.from_reflect(value.as_partial_reflect())
            {
                let mut concrete: Box<dyn PartialReflect> = concrete.into_partial_reflect();
                mapper.map_entities(concrete.as_mut(), &mut SnapshotMapper(&map));
                value = concrete;
            }
            reflect.insert(&mut entity, value.as_partial_reflect(), &registry);
        }
    }

    // A tracked resource the keyframe did not have (added since) goes.
    for entry in &entries {
        if !snapshot
            .resources
            .iter()
            .any(|saved| saved.type_path == entry.type_path)
        {
            (entry.remove)(world);
        }
    }
    for (entry, value) in resources {
        (entry.apply)(world, &registry, value, &mut SnapshotMapper(&map));
    }
    map.retain(|source, target| source != target);
    world.insert_resource(DemoRemap { moved: map });
    Ok(())
}

/// Runs one fixed step of `world` on the action `row` (module docs, "Stepping a world").
#[tracing::instrument(skip_all, target = "regia_framework", name = "demo_world_step")]
pub fn step_world(world: &mut World, actions: &[String], row: &[Vec3]) {
    // `First` advances the frame counter: the injection targets the frame about to run.
    let frame = world.get_resource::<Time>().map_or(0, Time::frame_count) + 1;
    if let Some(mut injection) = world.get_resource_mut::<InputInjection>() {
        for (index, name) in actions.iter().enumerate() {
            let value = row.get(index).copied().unwrap_or(Vec3::ZERO);
            injection.schedule(name.clone(), value, frame, 1);
        }
    }
    world.insert_resource(DemoCatchUp);
    for label in [First.intern(), PreUpdate.intern(), FixedUpdate.intern()] {
        if let Err(error) = world.try_run_schedule(label) {
            tracing::warn!(target: "regia_framework", "demo step could not run {label:?}: {error}");
        }
    }
    world.remove_resource::<DemoCatchUp>();
}

/// [`DemoSim`] over a live world (module docs). Borrow a world, seek or verify a demo, drop it.
pub struct WorldDemoSim<'w> {
    world: &'w mut World,
    actions: Vec<String>,
    /// The last snapshot error, if a keyframe could not carry a snapshot.
    pub snapshot_error: Option<String>,
}

impl<'w> WorldDemoSim<'w> {
    /// A sim stepping `world` with the demo's tracked `actions` (in input-row order).
    #[must_use]
    pub fn new(world: &'w mut World, actions: Vec<String>) -> Self {
        Self {
            world,
            actions,
            snapshot_error: None,
        }
    }

    /// The borrowed world.
    #[must_use]
    pub fn world(&self) -> &World {
        self.world
    }
}

impl DemoSim for WorldDemoSim<'_> {
    fn step(&mut self, inputs: &[Vec3]) {
        step_world(self.world, &self.actions, inputs);
    }

    fn state_hash(&self) -> u64 {
        world_demo_hash(self.world)
    }

    fn snapshot(&self) -> Vec<u8> {
        match world_demo_snapshot(self.world) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(target: "regia_framework", "demo keyframe without a snapshot: {error}");
                Vec::new()
            }
        }
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
        restore_world_demo(self.world, snapshot)
    }
}

/// A keyframe of the current world state at `tick`.
fn capture(world: &World, tick: u64) -> (DemoKeyframe, Option<String>) {
    let (snapshot, error) = match world_demo_snapshot(world) {
        Ok(bytes) => (bytes, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    (
        DemoKeyframe {
            tick,
            state_hash: world_demo_hash(world),
            snapshot_hex: crate::demo::encode_hex(&snapshot),
        },
        error,
    )
}

/// What the live driver is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DemoMode {
    /// Nothing loaded or recording.
    #[default]
    Idle,
    /// Recording the running game.
    Recording,
    /// Replaying a demo.
    Playing,
    /// A demo is loaded and paused (also after a divergence).
    Paused,
    /// Playback reached the last step (still loaded: seek to replay).
    Finished,
}

impl DemoMode {
    /// The protocol name (`idle`, `recording`, `playing`, `paused`, `finished`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Finished => "finished",
        }
    }
}

/// A request to the live driver, applied in `Last`.
#[derive(Debug, Clone, PartialEq)]
pub enum DemoRequest {
    /// Start recording under `header` (its `step_count` is filled in at the end), tracking
    /// `actions` (`None`: every action of the `InputConfig`, sorted).
    Record {
        /// Identity and keyframe interval.
        header: DemoHeader,
        /// Tracked actions.
        actions: Option<Vec<String>>,
    },
    /// Stop recording; the demo goes to [`DemoDriver::recording`] and, with a path, to disk
    /// (`.frec` by extension, RON otherwise).
    StopRecording {
        /// Where to save.
        path: Option<PathBuf>,
    },
    /// Load a demo and play it from tick 0 (the world is restored to the first keyframe).
    Play(Box<DemoRecording>),
    /// Pause (`true`) or resume (`false`) a loaded demo.
    Pause(bool),
    /// Put the world at this tick of the loaded demo (keeps the paused state).
    Seek(u64),
    /// Unload the demo and stop playback.
    Unload,
}

/// The result of the last seek, for status reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastSeek {
    /// The tick reached.
    pub tick: u64,
    /// The keyframe restored.
    pub keyframe_tick: u64,
    /// Steps replayed from it.
    pub steps_replayed: u64,
    /// Hashes compared.
    pub keyframes_checked: u32,
}

impl From<SeekReport> for LastSeek {
    fn from(report: SeekReport) -> Self {
        Self {
            tick: report.tick,
            keyframe_tick: report.keyframe_tick,
            steps_replayed: report.steps_replayed,
            keyframes_checked: report.keyframes_checked,
        }
    }
}

/// The live demo driver (module docs, "The live driver").
#[derive(Resource, Debug, Default)]
pub struct DemoDriver {
    mode: DemoMode,
    requests: Vec<DemoRequest>,
    /// Recording in progress: header and keyframes so far.
    header: Option<DemoHeader>,
    keyframes: Vec<DemoKeyframe>,
    next_keyframe: u64,
    /// The demo being played (or the one just recorded, once stopped).
    demo: Option<DemoRecording>,
    /// Keyframe ticks already compared during playback.
    checked_through: Option<u64>,
    /// Time::paused before the driver paused it.
    time_was_paused: bool,
    /// Last error code and message.
    last_error: Option<(String, String)>,
    /// Tick of the first keyframe whose hash differed during playback.
    diverged_at: Option<u64>,
    last_seek: Option<LastSeek>,
    saved_to: Option<PathBuf>,
}

impl DemoDriver {
    /// Queues a request; it is applied in `Last` of this frame (several seeks in one frame
    /// collapse to the last).
    pub fn request(&mut self, request: DemoRequest) {
        if let DemoRequest::Seek(_) = request {
            self.requests
                .retain(|queued| !matches!(queued, DemoRequest::Seek(_)));
        }
        self.requests.push(request);
    }

    /// Requests queued and not applied yet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.requests.len()
    }

    /// The current mode.
    #[must_use]
    pub fn mode(&self) -> DemoMode {
        self.mode
    }

    /// The loaded demo (playing) or the last one recorded.
    #[must_use]
    pub fn demo(&self) -> Option<&DemoRecording> {
        self.demo.as_ref()
    }

    /// The last finished recording (same as [`DemoDriver::demo`] after a stop).
    #[must_use]
    pub fn recording(&self) -> Option<&DemoRecording> {
        if self.header.is_none() {
            self.demo.as_ref()
        } else {
            None
        }
    }

    /// Keyframes captured so far while recording.
    #[must_use]
    pub fn recorded_keyframes(&self) -> &[DemoKeyframe] {
        &self.keyframes
    }

    /// The last error as `(code, message)`; codes are [`DemoError::code`] values plus `busy`
    /// (a request that conflicts with recording) and `no_demo` (nothing loaded).
    #[must_use]
    pub fn last_error(&self) -> Option<(&str, &str)> {
        self.last_error
            .as_ref()
            .map(|(code, message)| (code.as_str(), message.as_str()))
    }

    /// The tick of the first keyframe whose hash differed during playback.
    #[must_use]
    pub fn diverged_at(&self) -> Option<u64> {
        self.diverged_at
    }

    /// The last seek's report.
    #[must_use]
    pub fn last_seek(&self) -> Option<LastSeek> {
        self.last_seek
    }

    /// Where the last recording was saved.
    #[must_use]
    pub fn saved_to(&self) -> Option<&std::path::Path> {
        self.saved_to.as_deref()
    }

    fn fail(&mut self, code: &str, message: impl Into<String>) {
        let message = message.into();
        tracing::warn!(target: "regia_framework", "demo driver: {code}: {message}");
        self.last_error = Some((code.to_string(), message));
    }
}

/// A snapshot of the driver for UIs and the agent protocol.
#[derive(Debug, Clone, PartialEq)]
pub struct DemoStatus {
    /// Mode.
    pub mode: DemoMode,
    /// Current tick (recording: steps recorded; playback: steps played).
    pub tick: u64,
    /// Steps in the loaded demo (recording: so far).
    pub steps: u64,
    /// Tick rate of the demo.
    pub tick_rate_hz: u32,
    /// Keyframe ticks of the loaded demo (recording: so far).
    pub keyframe_ticks: Vec<u64>,
}

/// The driver's status in `world`, if the plugin is installed.
#[must_use]
pub fn demo_status(world: &World) -> Option<DemoStatus> {
    let driver = world.get_resource::<DemoDriver>()?;
    let recorder = world.get_resource::<InputRecorder>();
    let recorded = recorder.map_or(0, |r| r.recording.len() as u64);
    let cursor = recorder.map_or(0, |r| r.cursor as u64);
    Some(match driver.mode {
        DemoMode::Recording => DemoStatus {
            mode: driver.mode,
            tick: recorded,
            steps: recorded,
            tick_rate_hz: driver.header.as_ref().map_or(0, |h| h.tick_rate_hz),
            keyframe_ticks: driver.keyframes.iter().map(|k| k.tick).collect(),
        },
        mode => {
            let (steps, rate, keys) = driver.demo.as_ref().map_or((0, 0, Vec::new()), |demo| {
                (
                    demo.len(),
                    demo.header.tick_rate_hz,
                    demo.keyframes.iter().map(|k| k.tick).collect(),
                )
            });
            DemoStatus {
                mode,
                tick: if mode == DemoMode::Idle {
                    0
                } else {
                    cursor.min(steps)
                },
                steps,
                tick_rate_hz: rate,
                keyframe_ticks: keys,
            }
        }
    })
}

/// Installs [`DemoDriver`], [`DemoWorldConfig`], [`DemoTrackedResources`] and the driver system
/// (`Last`). Needs [`crate::input_recording::InputRecordingPlugin`] (added when missing) and the
/// input plugin.
pub struct DemoWorldPlugin;

impl Plugin for DemoWorldPlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<InputRecorder>().is_none() {
            app.add_plugins(crate::input_recording::InputRecordingPlugin);
        }
        // `ChildOf` is state (a seek puts a parent back); `Children` follows it.
        app.register_type::<DemoTracked>()
            .register_type::<ChildOf>()
            .init_resource::<DemoDriver>()
            .init_resource::<DemoWorldConfig>()
            .init_resource::<DemoTrackedResources>()
            .add_systems(Last, drive_demo);
    }
}

/// `Last`: applies queued requests, captures recording keyframes, checks playback keyframes.
#[tracing::instrument(skip_all, target = "regia_framework", name = "demo_drive")]
fn drive_demo(world: &mut World) {
    let Some(mut slot) = world.get_resource_mut::<DemoDriver>() else {
        return;
    };
    // A seek runs `PreUpdate` and `FixedUpdate` from here, so a system there may read the driver or
    // queue on it: a stand-in with the same mode stays in the world, and its queue carries over.
    let mode = slot.mode;
    let mut driver = std::mem::take(&mut *slot);
    slot.mode = mode;
    for request in std::mem::take(&mut driver.requests) {
        apply_request(world, &mut driver, request);
    }
    match driver.mode {
        DemoMode::Recording => record_keyframes(world, &mut driver),
        DemoMode::Playing => check_playback(world, &mut driver),
        _ => {}
    }
    if let Some(mut stand_in) = world.get_resource_mut::<DemoDriver>() {
        driver.requests.append(&mut stand_in.requests);
    }
    world.insert_resource(driver);
}

fn set_time_paused(world: &mut World, paused: bool) {
    if let Some(mut time) = world.get_resource_mut::<Time>() {
        time.paused = paused;
    }
}

fn time_paused(world: &World) -> bool {
    world.get_resource::<Time>().is_some_and(|time| time.paused)
}

fn apply_request(world: &mut World, driver: &mut DemoDriver, request: DemoRequest) {
    let recording = driver.mode == DemoMode::Recording;
    match request {
        DemoRequest::Record { header, actions } => {
            if recording {
                driver.fail("busy", "already recording");
                return;
            }
            let Some(mut recorder) = world.remove_resource::<InputRecorder>() else {
                driver.fail("no_demo", "no InputRecorder (add InputRecordingPlugin)");
                return;
            };
            match actions {
                Some(actions) => recorder.record_with(header.seed, actions),
                None => recorder.record(world, header.seed),
            }
            world.insert_resource(recorder);
            if driver.mode != DemoMode::Idle {
                set_time_paused(world, driver.time_was_paused);
            }
            let (key, error) = capture(world, 0);
            if let Some(error) = error {
                driver.fail("invalid", error);
            }
            driver.next_keyframe = u64::from(header.keyframe_interval);
            driver.keyframes = vec![key];
            driver.header = Some(header);
            driver.demo = None;
            driver.diverged_at = None;
            driver.mode = DemoMode::Recording;
        }
        DemoRequest::StopRecording { path } => {
            if !recording {
                driver.fail("no_demo", "not recording");
                return;
            }
            let input = {
                let Some(mut recorder) = world.get_resource_mut::<InputRecorder>() else {
                    driver.fail("no_demo", "the InputRecorder is gone");
                    return;
                };
                recorder.stop();
                recorder.recording.clone()
            };
            let end = input.len() as u64;
            if driver.keyframes.last().is_none_or(|key| key.tick != end) {
                let (key, error) = capture(world, end);
                if let Some(error) = error {
                    driver.fail("invalid", error);
                }
                driver.keyframes.push(key);
            }
            let mut header = driver
                .header
                .take()
                .unwrap_or_else(|| DemoHeader::new("", 0, 60, 0));
            // A session that reloaded extension code is not reproducible (ADR-9850 (b)).
            header.mark_nondeterministic(&crate::session_determinism::session_tags(world));
            let mut demo = DemoRecording::from_input_recording(header, &input);
            demo.keyframes = std::mem::take(&mut driver.keyframes);
            driver.mode = DemoMode::Idle;
            driver.saved_to = None;
            if let Some(path) = path {
                let saved = if path
                    .extension()
                    .is_some_and(|ext| ext == crate::input_recording::FREC_EXTENSION)
                {
                    crate::input_recording::save_frec(&demo, &path)
                } else {
                    demo.save(&path)
                };
                match saved {
                    Ok(()) => driver.saved_to = Some(path),
                    Err(error) => driver.fail(error.code(), error.to_string()),
                }
            }
            driver.demo = Some(demo);
        }
        DemoRequest::Play(demo) => {
            if recording {
                driver.fail("busy", "recording: stop it before playing a demo");
                return;
            }
            if let Err(error) = demo.validate() {
                driver.fail(error.code(), error.to_string());
                return;
            }
            if driver.mode == DemoMode::Idle {
                driver.time_was_paused = time_paused(world);
            }
            driver.demo = Some(*demo);
            driver.diverged_at = None;
            if seek_to(world, driver, 0) {
                set_time_paused(world, false);
                driver.mode = DemoMode::Playing;
            }
        }
        DemoRequest::Pause(pause) => {
            if driver.demo.is_none() || matches!(driver.mode, DemoMode::Idle | DemoMode::Recording) {
                driver.fail("no_demo", "no demo is loaded");
                return;
            }
            if pause {
                set_time_paused(world, true);
                driver.mode = DemoMode::Paused;
            } else if driver.mode != DemoMode::Finished {
                set_time_paused(world, false);
                driver.mode = DemoMode::Playing;
            }
        }
        DemoRequest::Seek(tick) => {
            if recording {
                driver.fail("busy", "recording: a seek would rewrite the recording");
                return;
            }
            if driver.demo.is_none() || driver.mode == DemoMode::Idle {
                driver.fail("no_demo", "no demo is loaded");
                return;
            }
            let resume = driver.mode == DemoMode::Playing;
            if seek_to(world, driver, tick) {
                let end = driver.demo.as_ref().map_or(0, DemoRecording::len);
                driver.mode = if tick >= end {
                    DemoMode::Finished
                } else if resume {
                    DemoMode::Playing
                } else {
                    DemoMode::Paused
                };
                set_time_paused(world, driver.mode != DemoMode::Playing);
            }
        }
        DemoRequest::Unload => {
            if recording {
                driver.fail("busy", "recording: stop it first");
                return;
            }
            if let Some(mut recorder) = world.get_resource_mut::<InputRecorder>() {
                recorder.stop();
            }
            if driver.mode != DemoMode::Idle {
                set_time_paused(world, driver.time_was_paused);
            }
            driver.mode = DemoMode::Idle;
            driver.demo = None;
            driver.checked_through = None;
        }
    }
}

/// Restores and replays the loaded demo to `tick`, then arms the recorder to continue from there.
/// Returns false (with the error recorded) when the seek failed; the world may then be at the
/// restored keyframe, and the driver pauses.
fn seek_to(world: &mut World, driver: &mut DemoDriver, tick: u64) -> bool {
    let Some(demo) = driver.demo.take() else {
        driver.fail("no_demo", "no demo is loaded");
        return false;
    };
    if let Some(mut recorder) = world.get_resource_mut::<InputRecorder>() {
        recorder.stop();
    }
    // Catch-up steps need a running clock.
    set_time_paused(world, false);
    let result = demo.seek(&mut WorldDemoSim::new(world, demo.actions.clone()), tick);
    let ok = match result {
        Ok(report) => {
            driver.last_seek = Some(report.into());
            driver.last_error = None;
            if let Some(mut recorder) = world.get_resource_mut::<InputRecorder>() {
                recorder.play(demo.to_input_recording());
                recorder.cursor = usize::try_from(tick).unwrap_or(usize::MAX);
                if recorder.cursor >= recorder.recording.frames.len() {
                    recorder.mode = RecorderMode::Idle;
                }
            }
            driver.checked_through = Some(tick);
            true
        }
        Err(error) => {
            if let DemoError::Diverged { tick, .. } = error {
                driver.diverged_at = Some(tick);
            }
            driver.fail(error.code(), error.to_string());
            set_time_paused(world, true);
            driver.mode = DemoMode::Paused;
            false
        }
    };
    driver.demo = Some(demo);
    ok
}

fn record_keyframes(world: &mut World, driver: &mut DemoDriver) {
    let steps = world
        .get_resource::<InputRecorder>()
        .map_or(0, |recorder| recorder.recording.len() as u64);
    let interval = driver
        .header
        .as_ref()
        .map_or(0, |h| u64::from(h.keyframe_interval));
    if interval == 0
        || steps < driver.next_keyframe
        || driver.keyframes.last().is_some_and(|k| k.tick >= steps)
    {
        return;
    }
    let (key, error) = capture(world, steps);
    if let Some(error) = error {
        driver.fail("invalid", error);
    }
    driver.keyframes.push(key);
    driver.next_keyframe = steps + interval;
}

fn check_playback(world: &mut World, driver: &mut DemoDriver) {
    let Some(recorder) = world.get_resource::<InputRecorder>() else {
        return;
    };
    let tick = recorder.cursor as u64;
    let Some(demo) = driver.demo.as_ref() else {
        return;
    };
    let end = demo.len();
    if driver.checked_through.is_none_or(|done| done < tick)
        && let Some(key) = demo.keyframes.iter().find(|key| key.tick == tick)
    {
        let expected = key.state_hash;
        let actual = world_demo_hash(world);
        driver.checked_through = Some(tick);
        if actual != expected {
            let error = DemoError::Diverged {
                tick,
                expected,
                actual,
            };
            driver.diverged_at = Some(tick);
            driver.fail(error.code(), error.to_string());
            set_time_paused(world, true);
            driver.mode = DemoMode::Paused;
            return;
        }
    }
    if tick >= end {
        set_time_paused(world, true);
        driver.mode = DemoMode::Finished;
    }
}

#[cfg(test)]
mod tests {
    use regia_ecs::ecs::entity::MapEntities;
    use regia_input::{ActionKind, InputConfig, InputPlugin};
    use regia_scene::components::Name;

    use super::*;
    use crate::input_recording::InputRecordingSet;

    /// A tracked mover: integrates `Move.x` and counts fixed steps.
    #[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Mover {
        x: f32,
        steps: u32,
    }

    /// A projectile spawned every fourth step while `Fire` is held; it remembers its shooter.
    #[derive(Component, Reflect, Debug, Clone, PartialEq)]
    #[reflect(Component, MapEntities)]
    struct Shot {
        shooter: Entity,
        age: u32,
    }

    impl Default for Shot {
        fn default() -> Self {
            Self {
                shooter: Entity::PLACEHOLDER,
                age: 0,
            }
        }
    }

    impl MapEntities for Shot {
        fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
            self.shooter = mapper.get_mapped(self.shooter);
        }
    }

    /// A tracked resource: a deterministic RNG and a score.
    #[derive(Resource, Reflect, Debug, Clone, Default, PartialEq)]
    #[reflect(Resource, Default)]
    struct Match {
        rng: u64,
        score: i64,
    }

    /// A presentation component that differs between runs and must not count.
    #[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Sparkle(u32);

    fn sim(
        mut commands: Commands,
        actions: Res<regia_input::Actions>,
        mut matched: ResMut<Match>,
        mut movers: Query<(Entity, &mut Mover)>,
        mut shots: Query<(Entity, &mut Shot)>,
    ) {
        let dx = actions.value("Move").x;
        let fire = actions.value("Fire").x > 0.5;
        for (_, mut shot) in &mut shots {
            shot.age += 1;
        }
        for (entity, shot) in &shots {
            if shot.age >= 6 {
                commands.entity(entity).despawn();
                matched.score += 1;
            }
        }
        for (entity, mut mover) in &mut movers {
            mover.x += dx * 0.5;
            mover.steps += 1;
            matched.rng = matched
                .rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(u64::from(mover.steps));
            if fire && mover.steps.is_multiple_of(4) {
                commands.spawn((
                    DemoTracked,
                    Name::new(format!("shot-{}", mover.steps)),
                    Shot {
                        shooter: entity,
                        age: 0,
                    },
                ));
            }
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins((
            InputPlugin::new(
                InputConfig::new()
                    .action("Move", ActionKind::Axis1D)
                    .action("Fire", ActionKind::Button),
            ),
            DemoWorldPlugin,
        ));
        app.register_type::<Mover>()
            .register_type::<Shot>()
            .register_type::<Sparkle>()
            .register_type::<Name>()
            .register_demo_resource::<Match>();
        app.insert_resource(Match { rng: 7, score: 0 });
        app.add_systems(FixedUpdate, sim.after(InputRecordingSet));
        enable_fixed_step(&mut app, 60.0);
        app.world_mut()
            .spawn((DemoTracked, Name::new("hero"), Mover::default(), Sparkle(0)));
        app.world_mut().spawn((Name::new("camera"), Sparkle(0)));
        app.update();
        app
    }

    fn row(k: u64) -> [Vec3; 2] {
        [
            Vec3::new(((k * 5) % 7) as f32 * 0.25 - 0.75, 0.0, 0.0),
            Vec3::new(if k % 9 < 5 { 1.0 } else { 0.0 }, 0.0, 0.0),
        ]
    }

    fn inject_row(app: &mut App, k: u64) {
        let frame = app.world().resource::<Time>().frame_count() + 1;
        let mut injection = app.world_mut().resource_mut::<InputInjection>();
        let [m, f] = row(k);
        injection.schedule("Move".to_string(), m, frame, 1);
        injection.schedule("Fire".to_string(), f, frame, 1);
    }

    fn driver(app: &mut App) -> Mut<'_, DemoDriver> {
        app.world_mut().resource_mut::<DemoDriver>()
    }

    /// Records `steps` steps through the live driver; returns the demo and the straight-run hash
    /// after every step.
    fn record(steps: u64, interval: u32) -> (DemoRecording, Vec<u64>) {
        let mut app = app();
        driver(&mut app).request(DemoRequest::Record {
            header: DemoHeader::new("yard", 7, 60, interval),
            actions: Some(vec!["Fire".to_string(), "Move".to_string()]),
        });
        app.update();
        let mut hashes = vec![world_demo_hash(app.world())];
        for k in 0..steps {
            inject_row(&mut app, k);
            // The stop applies in `Last`, after the frame's fixed step: queue it with the last
            // step so the demo has exactly `steps` steps.
            if k + 1 == steps {
                driver(&mut app).request(DemoRequest::StopRecording { path: None });
            }
            app.update();
            hashes.push(world_demo_hash(app.world()));
        }
        let demo = app
            .world()
            .resource::<DemoDriver>()
            .recording()
            .cloned()
            .expect("recording");
        (demo, hashes)
    }

    #[test]
    fn a_demo_of_a_session_marked_nondeterministic_carries_the_mark() {
        // ADR-9850 (b): an extension reload during the session marks the demo recorded in it.
        let mut app = app();
        driver(&mut app).request(DemoRequest::Record {
            header: DemoHeader::new("yard", 7, 60, 4).with_build("c0ffee"),
            actions: Some(vec!["Fire".to_string(), "Move".to_string()]),
        });
        app.update();
        inject_row(&mut app, 0);
        app.update();
        crate::session_determinism::mark_nondeterministic(
            app.world_mut(),
            crate::session_determinism::NONDETERMINISTIC_RELOAD,
            "live_ext",
            "generation 1",
        );
        inject_row(&mut app, 1);
        driver(&mut app).request(DemoRequest::StopRecording { path: None });
        app.update();
        let demo = app
            .world()
            .resource::<DemoDriver>()
            .recording()
            .cloned()
            .expect("recording");
        assert_eq!(
            demo.header.nondeterministic(),
            ["nondeterministic_reload:live_ext"]
        );
        assert_eq!(demo.header.build_id(), "c0ffee");
        // A clean session's demo has no mark.
        let (clean, _) = record(2, 4);
        assert!(clean.header.nondeterministic().is_empty());
    }

    #[test]
    fn recording_captures_rows_keyframes_and_the_initial_state() {
        let (demo, hashes) = record(30, 8);
        assert_eq!(demo.len(), 30);
        assert_eq!(demo.actions, vec!["Fire".to_string(), "Move".to_string()]);
        let ticks: Vec<u64> = demo.keyframes.iter().map(|k| k.tick).collect();
        assert_eq!(ticks, vec![0, 8, 16, 24, 30]);
        for key in &demo.keyframes {
            assert!(key.has_snapshot(), "tick {}", key.tick);
            assert_eq!(key.state_hash, hashes[key.tick as usize], "tick {}", key.tick);
        }
        assert_eq!(demo.steps[3], row(3).iter().rev().copied().collect::<Vec<_>>());
        assert_eq!(demo.validate(), Ok(()));
        assert_eq!(demo.timestamps.len(), 30, "the recorder's stamps are kept");
    }

    #[test]
    fn seeking_a_world_lands_on_the_straight_run_state_at_every_tick() {
        let (demo, hashes) = record(26, 6);
        let mut app = app();
        for tick in [0u64, 1, 5, 6, 7, 13, 25, 26, 3, 19] {
            let mut sim = WorldDemoSim::new(app.world_mut(), demo.actions.clone());
            let report = demo.seek(&mut sim, tick).expect("seek");
            assert_eq!(report.state_hash, hashes[tick as usize], "tick {tick}");
            assert_eq!(world_demo_hash(app.world()), hashes[tick as usize]);
        }
    }

    #[test]
    fn restore_respawns_despawned_entities_and_remaps_their_references() {
        let (demo, _) = record(20, 4);
        let mut app = app();
        // A shot is in flight at tick 11 (fired on step 9) and gone by tick 20: seeking back
        // respawns it.
        let mut sim = WorldDemoSim::new(app.world_mut(), demo.actions.clone());
        demo.seek(&mut sim, 20).expect("end");
        let hash_11 = demo.seek(&mut sim, 11).expect("back").state_hash;
        let find_hero = |world: &mut World| {
            world
                .query::<(Entity, &Name)>()
                .iter(world)
                .find(|(_, name)| name.as_str() == "hero")
                .map(|(entity, _)| entity)
        };
        let world = app.world_mut();
        let hero = find_hero(world).expect("hero");
        let shots: Vec<Shot> = world.query::<&Shot>().iter(world).cloned().collect();
        assert!(!shots.is_empty(), "the tick-11 state has a shot in flight");
        assert!(
            shots.iter().all(|shot| shot.shooter == hero),
            "{shots:?} -> {hero}"
        );

        // Now the shooter itself is gone: a restore respawns it under a new id and the shot's
        // reference follows it.
        let snapshot = world_demo_snapshot(world).expect("snapshot");
        let tracked: Vec<Entity> = world
            .query_filtered::<Entity, With<DemoTracked>>()
            .iter(world)
            .collect();
        for entity in tracked {
            world.despawn(entity);
        }
        world.spawn((DemoTracked, Name::new("intruder"), Mover::default()));
        restore_world_demo(world, &snapshot).expect("restore");
        let new_hero = find_hero(world).expect("hero respawned");
        assert_ne!(new_hero, hero, "a despawned entity comes back under a new id");
        let shots: Vec<Shot> = world.query::<&Shot>().iter(world).cloned().collect();
        assert!(!shots.is_empty());
        assert!(
            shots.iter().all(|shot| shot.shooter == new_hero),
            "{shots:?} -> {new_hero}"
        );
        assert_eq!(
            world_demo_hash(world),
            hash_11,
            "the restored state hashes as the original"
        );
    }

    #[test]
    fn a_respawned_child_rejoins_its_parent() {
        let mut app = app();
        app.register_type::<ChildOf>();
        let world = app.world_mut();
        let parent = world
            .spawn((DemoTracked, Name::new("rig"), Mover::default()))
            .id();
        let child = world
            .spawn((DemoTracked, Name::new("arm"), Mover::default(), ChildOf(parent)))
            .id();
        let before = world_demo_hash(world);
        let snapshot = world_demo_snapshot(world).expect("snapshot");
        world.despawn(child);
        assert!(world.get::<Children>(parent).is_none_or(|c| c.is_empty()));
        restore_world_demo(world, &snapshot).expect("restore");
        let children: Vec<Entity> = world
            .get::<Children>(parent)
            .map(|c| c.to_vec())
            .unwrap_or_default();
        assert_eq!(
            children.len(),
            1,
            "the respawned child is the parent's child again"
        );
        assert_ne!(children[0], child, "under a new id");
        assert_eq!(
            world.get::<ChildOf>(children[0]).map(ChildOf::parent),
            Some(parent)
        );
        assert_eq!(world_demo_hash(world), before);
    }

    #[test]
    fn presentation_components_and_untracked_entities_do_not_change_the_hash() {
        let mut app = app();
        let before = world_demo_hash(app.world());
        let world = app.world_mut();
        for mut sparkle in world.query::<&mut Sparkle>().iter_mut(world) {
            sparkle.0 += 1;
        }
        world.spawn((Name::new("decor"), Mover { x: 9.0, steps: 1 }));
        assert_ne!(
            before,
            world_demo_hash(app.world()),
            "Sparkle on the tracked hero counts"
        );
        app.world_mut()
            .resource_mut::<DemoWorldConfig>()
            .excluded_components
            .push("Sparkle".to_string());
        let mut fresh = self::app();
        fresh
            .world_mut()
            .resource_mut::<DemoWorldConfig>()
            .excluded_components
            .push("Sparkle".to_string());
        assert_eq!(
            world_demo_hash(app.world()),
            world_demo_hash(fresh.world()),
            "excluded components and untracked entities are outside the hash"
        );
    }

    #[test]
    fn a_tracked_resource_is_hashed_and_restored() {
        let mut app = app();
        let snapshot = world_demo_snapshot(app.world()).expect("snapshot");
        let before = world_demo_hash(app.world());
        app.world_mut().resource_mut::<Match>().score = 99;
        assert_ne!(before, world_demo_hash(app.world()));
        restore_world_demo(app.world_mut(), &snapshot).expect("restore");
        assert_eq!(app.world().resource::<Match>().score, 0);
        assert_eq!(before, world_demo_hash(app.world()));
    }

    #[test]
    fn a_bad_snapshot_is_refused_and_leaves_the_world_alone() {
        let mut app = app();
        let before = world_demo_hash(app.world());
        assert!(restore_world_demo(app.world_mut(), b"nonsense").is_err());
        let mut future = world_demo_snapshot(app.world()).expect("snapshot");
        future[0] = 9; // the version varint
        assert!(restore_world_demo(app.world_mut(), &future).is_err());
        assert_eq!(before, world_demo_hash(app.world()));
    }

    /// Rewrites the first resource blob of a snapshot.
    fn tamper_resource(bytes: &[u8], edit: impl Fn(&mut SnapshotResource)) -> Vec<u8> {
        let mut snapshot: WorldSnapshot = postcard::from_bytes(bytes).expect("snapshot");
        edit(&mut snapshot.resources[0]);
        postcard::to_allocvec(&snapshot).expect("encode")
    }

    #[test]
    fn a_bad_resource_in_a_snapshot_leaves_the_entities_alone() {
        let mut app = app();
        let good = world_demo_snapshot(app.world()).expect("snapshot");
        app.world_mut()
            .spawn((DemoTracked, Name::new("late"), Mover::default()));
        let before = world_demo_hash(app.world());

        let garbage = tamper_resource(&good, |resource| resource.data = vec![0xff; 3]);
        assert!(restore_world_demo(app.world_mut(), &garbage).is_err());
        assert_eq!(world_demo_hash(app.world()), before, "a bad blob changes nothing");

        let unknown = tamper_resource(&good, |resource| resource.type_path = "no::Such".to_string());
        let error = restore_world_demo(app.world_mut(), &unknown).expect_err("unknown resource");
        assert!(error.contains("no::Such"), "{error}");
        assert_eq!(
            world_demo_hash(app.world()),
            before,
            "an unknown type changes nothing"
        );

        restore_world_demo(app.world_mut(), &good).expect("the good snapshot still restores");
        assert_ne!(world_demo_hash(app.world()), before);
    }

    mod first {
        use super::*;
        /// Shares its short name with [`second::Tag`](super::second::Tag).
        #[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
        #[reflect(Component, Default)]
        pub struct Tag(pub u32);
    }

    mod second {
        use super::*;
        /// Shares its short name with [`first::Tag`](super::first::Tag).
        #[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
        #[reflect(Component, Default)]
        pub struct Tag(pub u32);
    }

    #[test]
    fn two_components_with_one_short_name_restore() {
        let mut app = app();
        app.register_type::<first::Tag>().register_type::<second::Tag>();
        let entity = app
            .world_mut()
            .spawn((DemoTracked, Name::new("tagged"), first::Tag(1), second::Tag(2)))
            .id();
        let saved = world_demo_snapshot(app.world()).expect("snapshot");
        app.world_mut()
            .entity_mut(entity)
            .insert((first::Tag(10), second::Tag(20)));
        restore_world_demo(app.world_mut(), &saved).expect("restore");
        assert_eq!(app.world().get::<first::Tag>(entity), Some(&first::Tag(1)));
        assert_eq!(app.world().get::<second::Tag>(entity), Some(&second::Tag(2)));
    }

    /// A tracked resource that points at an entity.
    #[derive(Resource, Reflect, Debug, Clone, PartialEq)]
    #[reflect(Resource, MapEntities)]
    struct Focus {
        target: Entity,
        text: String,
    }

    impl Default for Focus {
        fn default() -> Self {
            Self {
                target: Entity::PLACEHOLDER,
                text: String::new(),
            }
        }
    }

    impl MapEntities for Focus {
        fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
            self.target = mapper.get_mapped(self.target);
        }
    }

    #[test]
    fn a_resource_reference_follows_a_respawned_entity() {
        let mut app = app();
        app.register_demo_resource::<Focus>();
        let boss = app
            .world_mut()
            .spawn((DemoTracked, Name::new("boss"), Mover::default()))
            .id();
        app.insert_resource(Focus {
            target: boss,
            text: String::new(),
        });
        let saved = world_demo_snapshot(app.world()).expect("snapshot");
        let hash = world_demo_hash(app.world());
        app.world_mut().despawn(boss);
        restore_world_demo(app.world_mut(), &saved).expect("restore");
        let target = app.world().resource::<Focus>().target;
        assert_ne!(target, boss, "the boss came back under a new id");
        assert!(app.world().get::<Mover>(target).is_some());
        assert_eq!(world_demo_hash(app.world()), hash);
    }

    #[test]
    fn entity_like_text_in_a_tracked_resource_is_not_a_reference() {
        fn hash(spares: usize, token: Option<&str>) -> (u64, String) {
            let mut app = app();
            app.register_demo_resource::<Focus>();
            for _ in 0..spares {
                app.world_mut().spawn_empty();
            }
            let boss = app
                .world_mut()
                .spawn((DemoTracked, Name::new("boss"), Mover::default()))
                .id();
            let token = token.map_or_else(|| boss.to_string(), str::to_owned);
            app.insert_resource(Focus {
                target: boss,
                text: token.clone(),
            });
            (world_demo_hash(app.world()), token)
        }

        let (first, token) = hash(0, None);
        let (second, _) = hash(1, Some(&token));
        assert_eq!(first, second, "resource text stays {token:?} in both worlds");
    }

    #[test]
    fn a_snapshot_id_held_by_an_untracked_entity_gets_a_new_entity() {
        // Two apps spawn in the same order; in the second the third entity is not tracked.
        let mut recorded = app();
        let ghost = recorded
            .world_mut()
            .spawn((DemoTracked, Name::new("ghost"), Mover::default()))
            .id();
        let saved = world_demo_snapshot(recorded.world()).expect("snapshot");
        let hash = world_demo_hash(recorded.world());

        let mut fresh = app();
        let prop = fresh.world_mut().spawn((Name::new("prop"), Sparkle(3))).id();
        assert_eq!(prop, ghost, "the ids collide");
        restore_world_demo(fresh.world_mut(), &saved).expect("restore");
        assert!(!fresh.world().entity(prop).contains::<DemoTracked>());
        assert!(
            fresh.world().get::<Mover>(prop).is_none(),
            "the prop is untouched"
        );
        assert_eq!(fresh.world().get::<Sparkle>(prop), Some(&Sparkle(3)));
        assert_eq!(
            world_demo_hash(fresh.world()),
            hash,
            "the ghost came back elsewhere"
        );
    }

    #[test]
    fn a_restore_publishes_the_entities_that_came_back_under_new_ids() {
        let mut app = app();
        let mut heroes = app.world_mut().query_filtered::<Entity, With<Mover>>();
        let hero = heroes.single(app.world()).expect("hero");
        let saved = world_demo_snapshot(app.world()).expect("snapshot");
        restore_world_demo(app.world_mut(), &saved).expect("restore in place");
        assert!(
            app.world().resource::<DemoRemap>().moved.is_empty(),
            "nothing moved"
        );
        app.world_mut().despawn(hero);
        restore_world_demo(app.world_mut(), &saved).expect("restore");
        let remap = app.world().resource::<DemoRemap>();
        let back = remap.get(hero).expect("the hero moved");
        assert_ne!(back, hero);
        assert_eq!(remap.moved.len(), 1);
        assert!(app.world().get::<Mover>(back).is_some());
    }

    #[test]
    fn a_tracked_resource_added_after_the_keyframe_goes() {
        let mut app = app();
        app.register_demo_resource::<Focus>();
        let saved = world_demo_snapshot(app.world()).expect("snapshot");
        let hash = world_demo_hash(app.world());
        app.insert_resource(Focus::default());
        assert_ne!(world_demo_hash(app.world()), hash);
        restore_world_demo(app.world_mut(), &saved).expect("restore");
        assert!(!app.world().contains_resource::<Focus>());
        assert_eq!(world_demo_hash(app.world()), hash);
    }

    #[test]
    fn a_child_taken_along_by_a_despawned_parent_comes_back() {
        let mut app = app();
        let child = app
            .world_mut()
            .spawn((DemoTracked, Name::new("rider"), Mover::default()))
            .id();
        let saved = world_demo_snapshot(app.world()).expect("snapshot");
        let hash = world_demo_hash(app.world());
        let late = app.world_mut().spawn((DemoTracked, Name::new("late-mount"))).id();
        app.world_mut().entity_mut(child).insert(ChildOf(late));
        restore_world_demo(app.world_mut(), &saved).expect("restore");
        assert!(app.world().get_entity(late).is_err(), "the new parent is gone");
        assert_eq!(
            world_demo_hash(app.world()),
            hash,
            "the child is back, unparented"
        );
    }

    /// What a `PreUpdate` system saw while a catch-up ran.
    #[derive(Resource, Default)]
    struct CatchUpProbe {
        steps: u32,
        driver_missing: u32,
        modes: Vec<DemoMode>,
        request_on: Option<u32>,
    }

    fn probe_catch_up(
        catch_up: Option<Res<DemoCatchUp>>,
        driver: Option<ResMut<DemoDriver>>,
        mut probe: ResMut<CatchUpProbe>,
    ) {
        if catch_up.is_none() {
            return;
        }
        probe.steps += 1;
        let Some(mut driver) = driver else {
            probe.driver_missing += 1;
            return;
        };
        let mode = driver.mode();
        probe.modes.push(mode);
        if probe.request_on == Some(probe.steps) {
            driver.request(DemoRequest::Pause(true));
        }
    }

    #[test]
    fn the_driver_stays_in_the_world_during_a_catch_up_and_keeps_its_queue() {
        let (demo, _) = record(16, 8);
        let mut app = app();
        app.init_resource::<CatchUpProbe>();
        app.add_systems(PreUpdate, probe_catch_up);
        driver(&mut app).request(DemoRequest::Play(Box::new(demo)));
        app.update();
        driver(&mut app).request(DemoRequest::Pause(true));
        app.update();
        app.world_mut().resource_mut::<CatchUpProbe>().request_on = Some(2);
        driver(&mut app).request(DemoRequest::Seek(13));
        app.update();

        assert!(
            !app.world().contains_resource::<DemoCatchUp>(),
            "the marker is gone after the seek"
        );
        let probe = app.world().resource::<CatchUpProbe>();
        assert_eq!(probe.steps, 5, "keyframe 8 plus 5 steps");
        assert_eq!(probe.driver_missing, 0);
        assert!(probe.modes.iter().all(|mode| *mode == DemoMode::Paused));
        let driver = app.world().resource::<DemoDriver>();
        assert_eq!(driver.mode(), DemoMode::Paused);
        assert_eq!(
            driver.pending(),
            1,
            "the request queued mid-seek waits for the next frame"
        );
        assert!(driver.demo().is_some(), "the real driver is back");
    }

    #[test]
    fn unload_forgets_the_demo_and_restores_the_clock() {
        let (demo, _) = record(8, 4);
        let mut app = app();
        driver(&mut app).request(DemoRequest::Play(Box::new(demo)));
        app.update();
        driver(&mut app).request(DemoRequest::Pause(true));
        app.update();
        assert!(app.world().resource::<Time>().paused);
        driver(&mut app).request(DemoRequest::Unload);
        app.update();
        let driver = app.world().resource::<DemoDriver>();
        assert_eq!(driver.mode(), DemoMode::Idle);
        assert!(driver.demo().is_none());
        assert!(driver.recording().is_none());
        assert!(!app.world().resource::<Time>().paused);
        let status = demo_status(app.world()).expect("status");
        assert_eq!((status.tick, status.steps), (0, 0));
    }

    #[test]
    fn the_live_driver_plays_seeks_pauses_and_finishes() {
        let (demo, hashes) = record(24, 6);
        let mut app = app();
        driver(&mut app).request(DemoRequest::Play(Box::new(demo.clone())));
        app.update();
        let status = demo_status(app.world()).expect("status");
        assert_eq!(
            (status.mode, status.tick, status.steps),
            (DemoMode::Playing, 0, 24)
        );
        assert_eq!(
            world_demo_hash(app.world()),
            hashes[0],
            "play starts from the first keyframe"
        );
        for _ in 0..6 {
            app.update();
        }
        let status = demo_status(app.world()).expect("status");
        assert_eq!(status.tick, 6);
        assert_eq!(world_demo_hash(app.world()), hashes[6]);

        driver(&mut app).request(DemoRequest::Pause(true));
        app.update();
        let paused_at = demo_status(app.world()).expect("status").tick;
        app.update();
        app.update();
        assert_eq!(
            demo_status(app.world()).expect("status").tick,
            paused_at,
            "paused"
        );
        assert_eq!(world_demo_hash(app.world()), hashes[paused_at as usize]);

        // Seek while paused: lands exactly and stays paused.
        driver(&mut app).request(DemoRequest::Seek(17));
        app.update();
        let status = demo_status(app.world()).expect("status");
        assert_eq!((status.mode, status.tick), (DemoMode::Paused, 17));
        assert_eq!(world_demo_hash(app.world()), hashes[17]);
        let seek = app.world().resource::<DemoDriver>().last_seek().expect("seek");
        assert_eq!((seek.keyframe_tick, seek.steps_replayed), (12, 5));

        // Resume to the end.
        driver(&mut app).request(DemoRequest::Pause(false));
        for _ in 0..10 {
            app.update();
        }
        let status = demo_status(app.world()).expect("status");
        assert_eq!((status.mode, status.tick), (DemoMode::Finished, 24));
        assert_eq!(world_demo_hash(app.world()), hashes[24]);
        assert!(app.world().resource::<DemoDriver>().diverged_at().is_none());
        assert!(app.world().resource::<DemoDriver>().last_error().is_none());
    }

    #[test]
    fn the_driver_reports_a_divergence_and_pauses() {
        let (mut demo, _) = record(18, 6);
        // A keyframe the replay cannot match (a different build).
        demo.keyframes[2].state_hash ^= 1;
        demo.keyframes[2].snapshot_hex.clear();
        let mut app = app();
        driver(&mut app).request(DemoRequest::Play(Box::new(demo)));
        for _ in 0..20 {
            app.update();
        }
        let driver = app.world().resource::<DemoDriver>();
        assert_eq!(driver.diverged_at(), Some(12));
        assert_eq!(driver.last_error().map(|(code, _)| code), Some("diverged"));
        assert_eq!(driver.mode(), DemoMode::Paused);
    }

    #[test]
    fn requests_that_conflict_are_refused_with_a_code() {
        let mut app = app();
        driver(&mut app).request(DemoRequest::Seek(3));
        app.update();
        assert_eq!(
            app.world().resource::<DemoDriver>().last_error().map(|(c, _)| c),
            Some("no_demo")
        );
        driver(&mut app).request(DemoRequest::Record {
            header: DemoHeader::new("l", 0, 60, 4),
            actions: None,
        });
        app.update();
        assert_eq!(app.world().resource::<DemoDriver>().mode(), DemoMode::Recording);
        let (demo, _) = record(4, 2);
        driver(&mut app).request(DemoRequest::Play(Box::new(demo)));
        app.update();
        assert_eq!(
            app.world().resource::<DemoDriver>().last_error().map(|(c, _)| c),
            Some("busy")
        );
        assert_eq!(
            app.world()
                .resource::<DemoDriver>()
                .recorded_keyframes()
                .first()
                .map(|k| k.tick),
            Some(0)
        );
    }

    #[test]
    fn several_seeks_in_one_frame_collapse_to_the_last() {
        let mut driver = DemoDriver::default();
        driver.request(DemoRequest::Seek(1));
        driver.request(DemoRequest::Pause(true));
        driver.request(DemoRequest::Seek(5));
        assert_eq!(driver.pending(), 2);
    }

    #[test]
    fn a_recording_saves_as_frec_or_ron_by_extension() {
        let dir = regia_test_utils::unique_temp_dir("demo-world-save");
        for name in ["m.frec", "m.regiademo"] {
            let mut app = app();
            driver(&mut app).request(DemoRequest::Record {
                header: DemoHeader::new("yard", 1, 60, 3),
                actions: None,
            });
            for k in 0..7 {
                inject_row(&mut app, k);
                app.update();
            }
            let path = dir.join(name);
            driver(&mut app).request(DemoRequest::StopRecording {
                path: Some(path.clone()),
            });
            app.update();
            let driver = app.world().resource::<DemoDriver>();
            assert_eq!(driver.saved_to(), Some(path.as_path()));
            let loaded = DemoRecording::load_any(&path).expect("load");
            assert_eq!(Some(&loaded), driver.recording());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measurement, not a check (`cargo test -p regia_framework --lib -- --ignored
    /// demo_world::tests::measure --nocapture`): snapshot size and seek cost with 1,000 tracked
    /// entities.
    #[test]
    #[ignore = "prints measurements; run by hand"]
    fn measure_snapshot_and_seek_cost() {
        let mut app = app();
        for i in 0..1000 {
            app.world_mut().spawn((
                DemoTracked,
                Name::new(format!("crowd-{i}")),
                Mover {
                    x: i as f32,
                    steps: 0,
                },
            ));
        }
        let started = std::time::Instant::now();
        let snapshot = world_demo_snapshot(app.world()).expect("snapshot");
        let snap_time = started.elapsed();
        let started = std::time::Instant::now();
        let hash = world_demo_hash(app.world());
        let hash_time = started.elapsed();
        let started = std::time::Instant::now();
        restore_world_demo(app.world_mut(), &snapshot).expect("restore");
        let restore_time = started.elapsed();
        assert_eq!(hash, world_demo_hash(app.world()));
        let mut sim = WorldDemoSim::new(app.world_mut(), vec!["Fire".to_string(), "Move".to_string()]);
        let started = std::time::Instant::now();
        for k in 0..59 {
            sim.step(&row(k));
        }
        let steps_time = started.elapsed();
        println!(
            "1001 tracked entities: snapshot {} bytes in {snap_time:?}; hash {hash_time:?}; restore \
             {restore_time:?}; 59 catch-up steps {steps_time:?}",
            snapshot.len()
        );
    }

    #[test]
    fn mode_names_are_stable() {
        let names: Vec<&str> = [
            DemoMode::Idle,
            DemoMode::Recording,
            DemoMode::Playing,
            DemoMode::Paused,
            DemoMode::Finished,
        ]
        .into_iter()
        .map(DemoMode::name)
        .collect();
        assert_eq!(names, ["idle", "recording", "playing", "paused", "finished"]);
    }
}
