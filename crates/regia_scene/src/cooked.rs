// SPDX-License-Identifier: MPL-2.0
//! The cooked scene form (`docs/specs/14-scene-and-framework.md` A2): postcard with type paths
//! hashed into [`TypePathHash`]es plus a string table, for the runtime load path.
//!
//! The authoring `.level.ron` / `.prefab.ron` keeps component values as RON text (diff-friendly,
//! reflection-decoded on load). Cooking turns each value into postcard bytes once; loading a
//! cooked scene then skips text parsing entirely, which is what the 10,000-entity < 50 ms budget
//! is measured against. Values whose type is not registered at cook time stay as RON text
//! (`Payload::Raw`) and decode at load — the same leniency as the text format.

use std::collections::BTreeMap;
use std::ops::Range;

use regia_core::id::{Guid, TypePathHash};
use regia_ecs::prelude::*;
use regia_ecs::reflect::{AppTypeRegistry, TypeRegistration};
use regia_ecs::serialize::ReflectCodec;
use serde::{Deserialize, Serialize};

use crate::format::{PrefabInstanceData, SceneFile};
use crate::spawn::{SceneResourceHooks, SceneSpawnError, SpawnOptions, SpawnedScene};
use crate::transform::Transform;

/// The cooked scene format version (independent bump cadence from the text format; both are v1).
pub const COOKED_SCENE_VERSION: u32 = 1;

/// A cooked component (or resource) value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Payload {
    /// postcard bytes of the reflected value, plus the hash of its type path for a fast
    /// registry lookup. The type path itself sits in the string table.
    Postcard {
        /// FNV-1a of the type path (`TypePathHash::of`).
        type_hash: u64,
        /// The postcard-encoded reflected value.
        bytes: Vec<u8>,
    },
    /// RON text kept because the type was not registered at cook time.
    Raw(String),
}

/// How a cooked entity's parent is addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CookedParent {
    /// A parent inside the same scene (index into [`CookedScene::entities`]).
    Index(u32),
    /// A parent inside an instantiated prefab subtree (its local id).
    PrefabLocal(Guid),
}

/// One cooked entity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedEntity {
    /// Stable entity guid.
    pub guid: Guid,
    /// Name, interned into the string table.
    pub name: Option<u32>,
    /// Parent, if any.
    pub parent: Option<CookedParent>,
    /// Component type-path (string table index) → cooked value.
    pub components: Vec<(u32, Payload)>,
    /// Prefab instance record, cooked structurally (overrides stay RON — they are rare and
    /// small; their payload is not on the hot path).
    pub prefab: Option<PrefabInstanceData>,
}

/// The cooked, postcard-serializable form of a scene file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CookedScene {
    /// Cooked format version; loaders reject newer.
    pub format_version: u32,
    /// Interned strings: component type paths and entity names.
    pub strings: Vec<String>,
    /// Entities in file order; parents are indices.
    pub entities: Vec<CookedEntity>,
    /// Resource type path (string table index) → cooked value.
    pub resources: Vec<(u32, Payload)>,
}

/// A shared string table under construction.
#[derive(Default)]
struct StringTable {
    strings: Vec<String>,
    lookup: BTreeMap<String, u32>,
}

impl StringTable {
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(index) = self.lookup.get(text) {
            return *index;
        }
        let index = self.strings.len() as u32;
        self.strings.push(text.to_owned());
        self.lookup.insert(text.to_owned(), index);
        index
    }
}

impl CookedScene {
    /// Cooks a parsed scene file: every registered component/resource value becomes postcard
    /// bytes; unknown types stay as RON text with a warning collected in `warnings`.
    pub fn cook(file: &SceneFile, registry: &TypeRegistry) -> Result<(Self, Vec<String>), SceneSpawnError> {
        file.validate()?;
        let mut warnings = Vec::new();
        let mut table = StringTable::default();
        let mut codec = ReflectCodec::new(registry);

        let mut guid_to_index = BTreeMap::new();
        for (index, entity) in file.entities.iter().enumerate() {
            guid_to_index.insert(entity.guid, index as u32);
        }

        let mut entities = Vec::with_capacity(file.entities.len());
        for record in &file.entities {
            let mut components = Vec::with_capacity(record.components.len());
            for (type_path, raw) in &record.components {
                let index = table.intern(type_path);
                components.push((
                    index,
                    cook_value(type_path, raw.get_ron(), registry, &mut codec, &mut warnings),
                ));
            }
            entities.push(CookedEntity {
                guid: record.guid,
                name: record.name.as_deref().map(|name| table.intern(name)),
                parent: record.parent.map(|guid| match guid_to_index.get(&guid) {
                    Some(index) => CookedParent::Index(*index),
                    None => CookedParent::PrefabLocal(guid),
                }),
                components,
                prefab: record.prefab_instance.clone(),
            });
        }

        let mut resources = Vec::with_capacity(file.resources.len());
        for (type_path, raw) in &file.resources {
            let index = table.intern(type_path);
            resources.push((
                index,
                cook_value(type_path, raw.get_ron(), registry, &mut codec, &mut warnings),
            ));
        }

        Ok((
            Self {
                format_version: COOKED_SCENE_VERSION,
                strings: table.strings,
                entities,
                resources,
            },
            warnings,
        ))
    }

    /// Serializes to postcard bytes (what a pak stores).
    pub fn to_bytes(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Parses postcard bytes, rejecting newer format versions and version 0 (the format starts
    /// at 1, so a zero version is damage).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CookedSceneError> {
        let cooked: CookedScene = postcard::from_bytes(bytes)?;
        if cooked.format_version == 0 || cooked.format_version > COOKED_SCENE_VERSION {
            return Err(CookedSceneError::UnsupportedVersion(cooked.format_version));
        }
        Ok(cooked)
    }

    /// A string from the table (names, type paths; used in errors and by tools).
    pub fn string(&self, index: u32) -> Option<&str> {
        self.strings.get(index as usize).map(String::as_str)
    }

    /// The entity index ranges of whole hierarchies, ascending: every parent link stays inside one
    /// range, and together the ranges cover every entity. A cooked cell orders its entities so that
    /// each range is one hierarchy (`regia_asset_pipeline::stream_cook`); a scene that mixes
    /// hierarchies still gets a valid answer, with the mixed ones merged into one range.
    pub fn hierarchy_ranges(&self) -> Vec<Range<usize>> {
        // The furthest index each entity must stay with: its own, and for a parent link both ends.
        let count = self.entities.len();
        let mut reach: Vec<usize> = (0..count).collect();
        for (index, entity) in self.entities.iter().enumerate() {
            if let Some(CookedParent::Index(parent)) = entity.parent {
                let parent = parent as usize;
                if parent < count {
                    let (low, high) = (index.min(parent), index.max(parent));
                    reach[low] = reach[low].max(high);
                }
            }
        }
        let mut ranges = Vec::new();
        let mut start = 0;
        let mut end = 0;
        for (index, furthest) in reach.into_iter().enumerate() {
            end = end.max(furthest);
            if index == end {
                ranges.push(start..index + 1);
                start = index + 1;
            }
        }
        ranges
    }

    /// Spawns the entities of `range` (usually from [`Self::hierarchy_ranges`]), applying `remap` to
    /// the transform of every root, and links parents inside the range. Scene resources are not
    /// applied. A parent outside the range is reported as a warning and the entity stays a root.
    ///
    /// The batched form of [`Self::spawn`], for callers that spread one scene over several frames.
    pub fn spawn_range(
        &self,
        world: &mut World,
        range: Range<usize>,
        opts: &SpawnOptions,
        remap: &dyn Fn(Transform) -> Transform,
    ) -> Result<SpawnedRange, SceneSpawnError> {
        let _span = tracing::trace_span!("cooked_scene_spawn_range", entities = range.len()).entered();
        let range = range.start.min(self.entities.len())..range.end.min(self.entities.len());
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let registry = handle.read();
        let mut codec = ReflectCodec::new(&registry);
        let spawned = self.spawn_rows(world, range, opts, &registry, &mut codec)?;
        for root in &spawned.scene.roots {
            if let Some(mut transform) = world.get_mut::<Transform>(*root) {
                let moved = remap(*transform);
                *transform = moved;
            }
        }
        Ok(spawned)
    }

    /// Spawns the cooked scene. Values decode from postcard (or RON for unregistered-at-cook
    /// types); see `crate::spawn::spawn_scene` for the spawning rules themselves.
    pub fn spawn(&self, world: &mut World, opts: &SpawnOptions) -> Result<SpawnedScene, SceneSpawnError> {
        let _span = tracing::info_span!("cooked_scene_spawn").entered();
        // Reuse the text path by materialising a SceneFile with already-cooked values? No —
        // decode straight here to keep the load fast; the flows mirror `spawn_scene`.
        let registry_handle = world.resource::<AppTypeRegistry>().0.clone();
        let registry = registry_handle.read();
        let mut codec = ReflectCodec::new(&registry);
        let mut result = self
            .spawn_rows(world, 0..self.entities.len(), opts, &registry, &mut codec)?
            .scene;

        // Resources.
        let hooks = world
            .get_resource::<SceneResourceHooks>()
            .cloned()
            .unwrap_or_else(SceneResourceHooks::with_engine_hooks);
        for (type_index, payload) in &self.resources {
            let Some(type_path) = self.string(*type_index) else {
                result.warnings.push(format!(
                    "resource type-path string index {type_index} is out of range; skipped"
                ));
                continue;
            };
            let Some(registration) =
                regia_core::brand::lookup_type_path(type_path, |p| registry.get_with_type_path(p))
            else {
                result
                    .warnings
                    .push(format!("resource `{type_path}` is not registered; skipped"));
                continue;
            };
            let Some(hook) = hooks.get(type_path) else {
                result.warnings.push(format!(
                    "resource `{type_path}` has no `SceneResourceHooks` entry; skipped"
                ));
                continue;
            };
            match decode_value(payload, registration, &mut codec) {
                Ok(value) => {
                    if let Err(err) = (hook.apply)(world, value.as_partial_reflect()) {
                        result
                            .warnings
                            .push(format!("resource `{type_path}` failed to apply: {err}"));
                    }
                }
                Err(err) => result
                    .warnings
                    .push(format!("resource `{type_path}` failed to load: {err}")),
            }
        }

        Ok(result)
    }

    /// Spawns the entities of `range` with their components, prefab instances and parent links.
    fn spawn_rows(
        &self,
        world: &mut World,
        range: Range<usize>,
        opts: &SpawnOptions,
        registry: &TypeRegistry,
        codec: &mut ReflectCodec,
    ) -> Result<SpawnedRange, SceneSpawnError> {
        let first = range.start;
        let mut result = SpawnedScene::default();
        let mut prefab_entities: Vec<Entity> = Vec::new();

        use std::collections::HashMap;
        let mut rows: Vec<(Entity, &CookedEntity)> = Vec::with_capacity(range.len());
        for cooked in &self.entities[range] {
            let mut entity = world.spawn_empty();
            entity.insert(crate::components::EntityGuid(cooked.guid));
            if let Some(name_index) = cooked.name
                && let Some(name) = self.string(name_index)
            {
                entity.insert(crate::components::Name::new(name));
            }
            if let Some(level) = opts.level {
                entity.insert(crate::level::LevelMember::from_level(level));
            }
            if opts.persistent {
                entity.insert(crate::level::Persistent);
            }
            let entity_id = entity.id();
            rows.push((entity_id, cooked));
        }
        result.entities = rows.len();

        for (entity_id, cooked) in &rows {
            for (type_index, payload) in &cooked.components {
                let Some(type_path) = self.string(*type_index) else {
                    result.warnings.push(format!(
                        "entity {}: component type-path string index {type_index} is out of range; skipped",
                        cooked.guid
                    ));
                    continue;
                };
                let Some(registration) =
                    regia_core::brand::lookup_type_path(type_path, |p| registry.get_with_type_path(p))
                else {
                    result
                        .warnings
                        .push(format!("type `{type_path}` is not registered; skipped"));
                    continue;
                };
                let Some(reflect_component) = registration.data::<ReflectComponent>() else {
                    result
                        .warnings
                        .push(format!("type `{type_path}` has no ReflectComponent; skipped"));
                    continue;
                };
                match decode_value(payload, registration, codec) {
                    Ok(value) => {
                        // A value written by a newer build is not inserted, like the text path.
                        if let Err(err) =
                            crate::spawn::check_format_version(registration, value.as_partial_reflect())
                        {
                            result.warnings.push(format!(
                                "component `{type_path}` on {} skipped: {err}",
                                cooked.guid
                            ));
                            continue;
                        }
                        if let Ok(mut entity) = world.get_entity_mut(*entity_id) {
                            reflect_component.insert(&mut entity, &*value, registry);
                        }
                    }
                    Err(err) => result.warnings.push(format!(
                        "component `{type_path}` on {} failed to load: {err}",
                        cooked.guid
                    )),
                }
            }
        }

        // Prefab instances. Each local id remembers the row whose instance spawned it: a parent
        // link into a prefab subtree is a link below that row for the cycle check.
        let mut prefab_locals: HashMap<Guid, (Entity, usize)> = HashMap::new();
        for (row, (entity_id, cooked)) in rows.iter().enumerate() {
            if let Some(instance) = &cooked.prefab {
                match crate::prefab::instantiate_prefab(world, instance, *entity_id, opts, 0) {
                    Ok(spawned) => {
                        result.entities += spawned.entities_spawned;
                        result.warnings.extend(spawned.warnings);
                        prefab_entities.extend(spawned.locals.values().copied());
                        prefab_locals.extend(
                            spawned
                                .locals
                                .into_iter()
                                .map(|(local, entity)| (local, (entity, row))),
                        );
                    }
                    Err(SceneSpawnError::PrefabNotLoaded(prefab)) => {
                        result
                            .warnings
                            .push(format!("prefab {prefab} not loaded; instance is pending"));
                        if let Ok(mut entity) = world.get_entity_mut(*entity_id) {
                            entity.insert(crate::prefab::PendingPrefab::for_record(instance, None, opts));
                        }
                    }
                    Err(err) => {
                        // The caller gets no list of what spawned, so nothing may stay behind.
                        for (entity, _) in &rows {
                            if let Ok(entity_mut) = world.get_entity_mut(*entity) {
                                entity_mut.despawn();
                            }
                        }
                        return Err(err);
                    }
                }
            }
        }

        // Parenting. Cooked bytes are not validated like a scene file (guids may repeat, parents
        // may loop), so indices resolve to rows directly and a link that would close a parent
        // cycle is skipped with a warning.
        let resolve = |cooked: &CookedEntity| match cooked.parent {
            Some(CookedParent::Index(index)) => {
                let row = (index as usize).checked_sub(first)?;
                rows.get(row).map(|(parent, _)| (*parent, row))
            }
            Some(CookedParent::PrefabLocal(guid)) => prefab_locals.get(&guid).copied(),
            None => None,
        };
        let anchors: Vec<Option<usize>> = rows
            .iter()
            .map(|(_, cooked)| resolve(cooked).map(|(_, row)| row))
            .collect();
        let closes_cycle = crate::spawn::cycle_closing_links(&anchors);
        for (row, (entity_id, cooked)) in rows.iter().enumerate() {
            if cooked.parent.is_none() {
                continue;
            }
            match resolve(cooked) {
                Some(_) if closes_cycle[row] => result.warnings.push(format!(
                    "entity {}: cooked parent would close a parent cycle; left as a root",
                    cooked.guid
                )),
                Some((parent, _)) => {
                    if let Ok(mut entity) = world.get_entity_mut(*entity_id) {
                        entity.insert(ChildOf(parent));
                    }
                }
                None => result
                    .warnings
                    .push(format!("entity {}: cooked parent not found", cooked.guid)),
            }
        }

        for entity_id in rows.iter().map(|(entity, _)| *entity) {
            if world.get::<ChildOf>(entity_id).is_none() {
                result.roots.push(entity_id);
            }
        }

        let mut entities: Vec<Entity> = rows.iter().map(|(entity, _)| *entity).collect();
        prefab_entities.sort_unstable();
        entities.append(&mut prefab_entities);
        Ok(SpawnedRange {
            scene: result,
            entities,
        })
    }
}

/// What [`CookedScene::spawn_range`] spawned.
#[derive(Clone, Debug, Default)]
pub struct SpawnedRange {
    /// Roots, entity count and warnings, as [`CookedScene::spawn`] reports them.
    pub scene: SpawnedScene,
    /// Every entity spawned: the range's rows in order, then prefab internals.
    pub entities: Vec<Entity>,
}

/// Errors of the cooked form.
#[derive(Debug, thiserror::Error)]
pub enum CookedSceneError {
    /// postcard (de)serialization failed.
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    /// The bytes carry a newer format version than this build reads.
    #[error("cooked scene version {0} is newer than supported")]
    UnsupportedVersion(u32),
}

/// Cooks one value.
fn cook_value(
    type_path: &str,
    raw: &str,
    registry: &TypeRegistry,
    codec: &mut ReflectCodec,
    warnings: &mut Vec<String>,
) -> Payload {
    let Some(registration) =
        regia_core::brand::lookup_type_path(type_path, |p| registry.get_with_type_path(p))
    else {
        warnings.push(format!(
            "cook: type `{type_path}` not registered; keeping RON text"
        ));
        return Payload::Raw(raw.to_owned());
    };
    if registration.data::<ReflectComponent>().is_none() && registration.data::<ReflectResource>().is_none() {
        warnings.push(format!(
            "cook: type `{type_path}` has no component/resource data; keeping RON"
        ));
        return Payload::Raw(raw.to_owned());
    }
    match codec.from_ron_typed(raw, registration) {
        Ok(value) => {
            // Do not store a value written by a newer build as a live postcard value; keep the
            // RON text so a future build (or the spawn-time check) can still refuse it.
            if let Err(err) = crate::spawn::check_format_version(registration, value.as_partial_reflect()) {
                warnings.push(format!("cook: `{type_path}` skipped: {err}; keeping RON"));
                return Payload::Raw(raw.to_owned());
            }
            match codec.to_postcard_typed(value.as_partial_reflect()) {
                // A type with its own serde that skips a field (`skip_serializing_if`, e.g.
                // `LevelSettings::terrain` when `None`) writes postcard it cannot read back:
                // postcard is positional. Keep the RON text for such a value (ADR-5608).
                Ok(bytes) if codec.from_postcard_typed(&bytes, registration).is_err() => {
                    Payload::Raw(raw.to_owned())
                }
                Ok(bytes) => Payload::Postcard {
                    type_hash: TypePathHash::of(type_path).get(),
                    bytes,
                },
                Err(err) => {
                    warnings.push(format!(
                        "cook: `{type_path}` failed to encode: {err}; keeping RON"
                    ));
                    Payload::Raw(raw.to_owned())
                }
            }
        }
        Err(err) => {
            warnings.push(format!("cook: `{type_path}` failed to parse: {err}; keeping RON"));
            Payload::Raw(raw.to_owned())
        }
    }
}

/// Decodes one cooked value.
fn decode_value(
    payload: &Payload,
    registration: &TypeRegistration,
    codec: &mut ReflectCodec,
) -> Result<Box<dyn Reflect>, String> {
    match payload {
        Payload::Postcard { bytes, .. } => codec
            .from_postcard_typed(bytes, registration)
            .map_err(|err| err.to_string()),
        Payload::Raw(raw) => codec
            .from_ron_typed(raw, registration)
            .map_err(|err| err.to_string()),
    }
}

/// Convenience: cook a scene file from a world (registry via [`AppTypeRegistry`]).
pub fn cook_scene(world: &World, file: &SceneFile) -> Result<(CookedScene, Vec<String>), SceneSpawnError> {
    let handle = world.resource::<AppTypeRegistry>().0.clone();
    let registry = handle.read();
    CookedScene::cook(file, &registry)
}

/// Convenience: cook, serialize, parse and spawn in one step (tests, tools).
pub fn round_trip_cooked(
    world: &mut World,
    file: &SceneFile,
    opts: &SpawnOptions,
) -> Result<(CookedScene, SpawnedScene), SceneSpawnError> {
    let (cooked, _) = cook_scene(world, file)?;
    let bytes = cooked
        .to_bytes()
        .map_err(|err| SceneSpawnError::Invalid(err.to_string()))?;
    let parsed = CookedScene::from_bytes(&bytes).map_err(|err| SceneSpawnError::Invalid(err.to_string()))?;
    let spawned = parsed.spawn(world, opts)?;
    Ok((parsed, spawned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;

    fn test_world() -> World {
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            crate::components::register_primitives(&mut registry);
            registry.register::<Transform>();
            registry.register::<Marker>();
        }
        world.init_resource::<crate::prefab::PrefabLibrary>();
        world
    }

    #[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
    #[reflect(Component, Default)]
    struct Marker {
        value: f32,
    }

    #[test]
    fn cooked_spawn_reports_an_out_of_range_string_index() {
        let root = Guid::new_random();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Root", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0)),
        }}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let mut world = test_world();
        let (mut cooked, _) = cook_scene(&world, &file).expect("cook");
        cooked.entities[0].components[0].0 = u32::MAX;
        let spawned = cooked.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
        assert!(
            spawned
                .warnings
                .iter()
                .any(|w| w.contains(&root.to_string()) && w.contains("out of range")),
            "{:?}",
            spawned.warnings
        );
    }

    /// Audit fix lane (scene correctness, finding 2): a component whose `format_version` is
    /// newer than its `FormatVersion::CURRENT` stays out of the world on the cooked paths too,
    /// not only in `spawn_components`. Cooking keeps the raw text (no live value is stored),
    /// and the cooked spawn skips the insert with a warning; the text path agrees.
    #[test]
    fn cooked_and_text_spawn_skip_a_component_newer_than_its_format_version() {
        let root = Guid::new_random();
        let mut world = test_world();
        {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            registry.register::<crate::shape::Shape>();
        }
        // A Shape written by a newer build: same fields, `format_version` one above CURRENT.
        let shape_ron = {
            let handle = world.resource::<AppTypeRegistry>().0.clone();
            let registry = handle.read();
            let value = crate::shape::Shape {
                format_version: crate::shape::SHAPE_FORMAT_VERSION + 1,
                ..Default::default()
            };
            let mut codec = ReflectCodec::new(&registry);
            codec.to_ron_typed(&value).expect("serialize shape")
        };
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", components: {{
            "regia_scene::Shape": {shape_ron},
        }}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");

        // The text path already refuses the component: a warning, no `Shape` on the entity.
        let spawned_text =
            crate::spawn::spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("text spawn");
        let root_text = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == root)
            .map(|(e, _)| e)
            .expect("text root");
        assert!(
            world.get::<crate::shape::Shape>(root_text).is_none(),
            "text spawn leaves a newer-version component out"
        );
        assert!(
            spawned_text.warnings.iter().any(|w| w.contains("format_version")),
            "{:?}",
            spawned_text.warnings
        );

        // The cooked path agrees: cooking warns and keeps the raw text instead of a live
        // value, and the cooked spawn inserts no `Shape` either.
        let (cooked, cook_warnings) = cook_scene(&world, &file).expect("cook");
        assert!(
            cook_warnings.iter().any(|w| w.contains("format_version")),
            "{cook_warnings:?}"
        );
        let bytes = cooked.to_bytes().expect("bytes");
        let parsed = CookedScene::from_bytes(&bytes).expect("parse cooked");
        let mut world2 = test_world();
        {
            let handle = world2.resource::<AppTypeRegistry>().0.clone();
            let mut registry = handle.write();
            registry.register::<crate::shape::Shape>();
        }
        let spawned_cooked = parsed
            .spawn(&mut world2, &SpawnOptions::default())
            .expect("cooked spawn");
        let root_cooked = world2
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world2)
            .find(|(_, g)| g.0 == root)
            .map(|(e, _)| e)
            .expect("cooked root");
        assert!(
            world2.get::<crate::shape::Shape>(root_cooked).is_none(),
            "cooked spawn leaves a newer-version component out"
        );
        assert!(
            spawned_cooked
                .warnings
                .iter()
                .any(|w| w.contains("format_version")),
            "{:?}",
            spawned_cooked.warnings
        );
    }

    #[test]
    fn cook_load_matches_text_load() {
        let root = Guid::new_random();
        let child = Guid::new_random();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{root}", name: "Root", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 2.0, 3.0)),
            "regia_scene::cooked::tests::Marker": (value: 7.5),
        }}),
        (guid: "{child}", name: "Child", parent: Some("{root}"), components: {{
            "regia_scene::transform::Transform": (),
        }}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");

        let mut world = test_world();
        let (_, cooked, spawned) = {
            let (cooked, warnings) = cook_scene(&world, &file).expect("cook");
            assert!(warnings.is_empty(), "{warnings:?}");
            let bytes = cooked.to_bytes().expect("bytes");
            let parsed = CookedScene::from_bytes(&bytes).expect("parse");
            let spawned = parsed.spawn(&mut world, &SpawnOptions::default()).expect("spawn");
            (cooked, parsed, spawned)
        };
        let _ = cooked;

        assert_eq!(spawned.entities, 2);
        assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
        // Values decoded from postcard.
        let root_entity = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == root)
            .map(|(e, _)| e)
            .expect("root");
        let marker = world.get::<Marker>(root_entity).expect("marker");
        assert_eq!(marker.value, 7.5);
        let transform = world.get::<Transform>(root_entity).expect("transform");
        assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
        // Hierarchy intact.
        let child_entity = world
            .query::<(Entity, &crate::components::EntityGuid)>()
            .iter(&world)
            .find(|(_, g)| g.0 == child)
            .map(|(e, _)| e)
            .expect("child");
        assert_eq!(
            world.get::<ChildOf>(child_entity).expect("parent").parent(),
            root_entity
        );
    }

    #[test]
    fn unregistered_types_survive_cooking_as_raw() {
        let guid = Guid::new_random();
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{guid}", components: {{
            "my_game::Later": (x: 1),
        }}),
    ],
)
"#
        );
        let file = SceneFile::from_ron_str(&text).expect("parse");
        let world = test_world();
        let (cooked, warnings) = cook_scene(&world, &file).expect("cook");
        assert!(warnings.iter().any(|w| w.contains("my_game::Later")));
        let bytes = cooked.to_bytes().expect("bytes");
        let parsed = CookedScene::from_bytes(&bytes).expect("parse");
        assert!(matches!(parsed.entities[0].components[0].1, Payload::Raw(_)));
    }

    /// Two hierarchies (a root with a child, then a lone root) plus a third whose child comes first.
    fn three_hierarchies() -> (SceneFile, [Guid; 5]) {
        let ids: [Guid; 5] = std::array::from_fn(|_| Guid::new_random());
        let [a, a_child, b, c_child, c] = ids;
        let text = format!(
            r#"
(
    format_version: 1,
    entities: [
        (guid: "{a}", name: "A", components: {{
            "regia_scene::transform::Transform": (translation: (1.0, 0.0, 0.0)),
        }}),
        (guid: "{a_child}", name: "AChild", parent: Some("{a}"), components: {{
            "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0)),
        }}),
        (guid: "{b}", name: "B", components: {{
            "regia_scene::transform::Transform": (translation: (2.0, 0.0, 0.0)),
            "regia_scene::cooked::tests::Marker": (value: 2.0),
        }}),
        (guid: "{c_child}", name: "CChild", parent: Some("{c}"), components: {{
            "regia_scene::transform::Transform": (),
        }}),
        (guid: "{c}", name: "C", components: {{
            "regia_scene::transform::Transform": (translation: (3.0, 0.0, 0.0)),
        }}),
    ],
)
"#
        );
        (SceneFile::from_ron_str(&text).expect("parse"), ids)
    }

    #[test]
    fn hierarchy_ranges_keep_every_parent_link_inside_one_range() {
        let (file, _) = three_hierarchies();
        let world = test_world();
        let (cooked, _) = cook_scene(&world, &file).expect("cook");
        assert_eq!(cooked.hierarchy_ranges(), vec![0..2, 2..3, 3..5]);
    }

    #[test]
    fn hierarchy_ranges_merge_interleaved_hierarchies() {
        let (file, _) = three_hierarchies();
        let world = test_world();
        let (mut cooked, _) = cook_scene(&world, &file).expect("cook");
        // Move the child of entity 0 to the far end: everything between joins one range.
        cooked.entities[1].parent = None;
        cooked.entities[4].parent = Some(CookedParent::Index(0));
        cooked.entities[3].parent = None;
        assert_eq!(cooked.hierarchy_ranges(), vec![0..5]);
        // A parent index out of range is ignored here; the spawn warns about it.
        cooked.entities[4].parent = Some(CookedParent::Index(99));
        assert_eq!(cooked.hierarchy_ranges(), vec![0..1, 1..2, 2..3, 3..4, 4..5]);
        let empty = CookedScene {
            format_version: COOKED_SCENE_VERSION,
            strings: vec![],
            entities: vec![],
            resources: vec![],
        };
        assert!(empty.hierarchy_ranges().is_empty());
    }

    #[test]
    fn spawning_every_range_matches_one_spawn_and_remaps_only_roots() {
        let (file, ids) = three_hierarchies();
        let mut world = test_world();
        let (cooked, _) = cook_scene(&world, &file).expect("cook");
        let lift = |mut t: Transform| {
            t.translation.y += 100.0;
            t
        };
        let mut total = 0;
        for range in cooked.hierarchy_ranges() {
            let spawned = cooked
                .spawn_range(&mut world, range, &SpawnOptions::default(), &lift)
                .expect("spawn");
            assert!(spawned.scene.warnings.is_empty(), "{:?}", spawned.scene.warnings);
            assert_eq!(spawned.scene.roots.len(), 1);
            total += spawned.entities.len();
        }
        assert_eq!(total, 5);
        let find = |world: &mut World, guid: Guid| {
            world
                .query::<(Entity, &crate::components::EntityGuid)>()
                .iter(world)
                .find(|(_, g)| g.0 == guid)
                .map(|(e, _)| e)
                .expect("spawned")
        };
        let [a, a_child, b, c_child, c] = ids;
        let (ea, eac, eb, ecc, ec) = (
            find(&mut world, a),
            find(&mut world, a_child),
            find(&mut world, b),
            find(&mut world, c_child),
            find(&mut world, c),
        );
        // Roots moved by the remap; children stayed relative to their parents.
        assert_eq!(
            world.get::<Transform>(ea).expect("t").translation,
            Vec3::new(1.0, 100.0, 0.0)
        );
        assert_eq!(
            world.get::<Transform>(eac).expect("t").translation,
            Vec3::new(0.0, 1.0, 0.0)
        );
        assert_eq!(world.get::<ChildOf>(eac).expect("linked").parent(), ea);
        assert_eq!(world.get::<ChildOf>(ecc).expect("linked").parent(), ec);
        assert_eq!(world.get::<Marker>(eb).expect("marker").value, 2.0);
    }

    #[test]
    fn a_parent_outside_the_range_is_a_warning_not_a_link() {
        let (file, _) = three_hierarchies();
        let mut world = test_world();
        let (cooked, _) = cook_scene(&world, &file).expect("cook");
        let spawned = cooked
            .spawn_range(&mut world, 1..2, &SpawnOptions::default(), &|t| t)
            .expect("spawn");
        assert_eq!(spawned.scene.roots.len(), 1);
        assert!(
            spawned
                .scene
                .warnings
                .iter()
                .any(|w| w.contains("parent not found")),
            "{:?}",
            spawned.scene.warnings
        );
    }

    #[test]
    fn rejects_newer_version() {
        let cooked = CookedScene {
            format_version: COOKED_SCENE_VERSION + 1,
            strings: vec![],
            entities: vec![],
            resources: vec![],
        };
        let bytes = cooked.to_bytes().expect("bytes");
        assert!(CookedScene::from_bytes(&bytes).is_err());
    }

    /// audit-v034b-formats L5: version 0 never existed (the format starts at 1), so a zeroed
    /// version word is damage, not a current file.
    #[test]
    fn rejects_version_zero() {
        let cooked = CookedScene {
            format_version: 0,
            strings: vec![],
            entities: vec![],
            resources: vec![],
        };
        let bytes = cooked.to_bytes().expect("bytes");
        assert!(matches!(
            CookedScene::from_bytes(&bytes),
            Err(CookedSceneError::UnsupportedVersion(0))
        ));
    }
}
