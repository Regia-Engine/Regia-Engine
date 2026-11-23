// SPDX-License-Identifier: MPL-2.0
//! Configuration warnings (`docs/specs/19-editor.md` § 2, ADR-7772): a registry of checks per
//! component type, each `fn(&World, Entity) -> Option<ConfigWarning>`, run when an entity changes
//! (change ticks and structural changes), never every frame.
//!
//! The Outliner shows a warning badge on the row of a warned entity and a dimmed one on its
//! ancestors (hover: the text and the fix hint); the Problems panel lists the warnings; the agent
//! reads them with `editor.config_warnings`.
//!
//! Seed checks: a rigid body without a collider (on itself or a child), a collider of zero size, a
//! light with zero intensity or range, an audio emitter without a sound, a camera whose near plane
//! is not in front of its far plane, a mesh source without a mesh, and a prefab instance whose
//! source prefab is missing.

use std::any::TypeId;
use std::collections::{BTreeMap, HashSet};

use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::prelude::*;

/// One configuration warning on an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// Stable code (lowercase ascii, digits and `_`), for example `rigid_body_without_collider`.
    pub code: &'static str,
    /// What is wrong, in a sentence.
    pub text: String,
    /// What to do about it.
    pub fix_hint: String,
}

impl ConfigWarning {
    /// A warning from its parts.
    #[must_use]
    pub fn new(code: &'static str, text: impl Into<String>, fix_hint: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
            fix_hint: fix_hint.into(),
        }
    }
}

/// A check: the warning `entity` earns, if any.
pub type CheckFn = fn(&World, Entity) -> Option<ConfigWarning>;

/// The entities holding a `T` that was added or changed after a tick (a monomorphized helper the
/// registry stores per component type).
type ChangedFn = fn(&mut World, Tick) -> Vec<Entity>;

/// One registered check.
#[derive(Clone)]
struct Registered {
    /// The component the check is about (short type name, for listings).
    component: &'static str,
    /// The component's type.
    type_id: TypeId,
    /// The check.
    check: CheckFn,
}

/// The configuration-check registry and its last results (a world resource).
///
/// Register more checks with [`ConfigChecks::register`]; [`update`] re-runs the checks of the
/// entities that changed since the last update.
#[derive(Resource, Clone)]
pub struct ConfigChecks {
    /// Every check, in registration order.
    checks: Vec<Registered>,
    /// One change finder per watched component type.
    watched: Vec<(TypeId, ChangedFn)>,
    /// The current warnings of each warned entity.
    warnings: BTreeMap<Entity, Vec<ConfigWarning>>,
    /// The ancestors of warned entities (their rows get the dimmed badge).
    ancestors: HashSet<Entity>,
    /// The change tick of the last update.
    since: Option<Tick>,
    /// The structure signature of the last update.
    signature: Option<u64>,
    /// The archetype (index) of each watched entity at the last update.
    archetypes: std::collections::HashMap<Entity, u32>,
    /// Entity evaluations so far (every check of one entity counts one).
    evaluations: u64,
    /// Updates that evaluated at least one entity.
    passes: u64,
    /// Bumped whenever the warnings changed (the Problems panel and the rows key on it).
    revision: u64,
}

impl std::fmt::Debug for ConfigChecks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigChecks")
            .field("checks", &self.checks.len())
            .field("warned", &self.warnings.len())
            .field("evaluations", &self.evaluations)
            .finish_non_exhaustive()
    }
}

impl Default for ConfigChecks {
    fn default() -> Self {
        let mut checks = Self {
            checks: Vec::new(),
            watched: Vec::new(),
            warnings: BTreeMap::new(),
            ancestors: HashSet::new(),
            since: None,
            signature: None,
            archetypes: std::collections::HashMap::new(),
            evaluations: 0,
            passes: 0,
            revision: 0,
        };
        checks.register::<regia_physics::RigidBody>("RigidBody", check_rigid_body);
        // A collider on a child of a body changes the body's warning.
        checks.watch::<regia_physics::Collider>();
        // A child added or removed re-checks the parent (and moves the ancestor badges).
        checks.watch::<Children>();
        checks.register::<regia_physics::Collider>("Collider", check_collider);
        checks.register::<regia_render::PointLight>("PointLight", check_point_light);
        checks.register::<regia_render::SpotLight>("SpotLight", check_spot_light);
        checks.register::<regia_render::RectLight>("RectLight", check_rect_light);
        checks.register::<regia_render::TubeLight>("TubeLight", check_tube_light);
        checks.register::<regia_render::DirectionalLight>("DirectionalLight", check_directional_light);
        checks.register::<regia_audio::AudioEmitter>("AudioEmitter", check_audio_emitter);
        checks.register::<regia_render::Camera>("Camera", check_camera);
        checks.register::<regia_render::MeshSource>("MeshSource", check_mesh_source);
        checks.register::<regia_scene::prefab::PrefabInstance>("PrefabInstance", check_prefab_source);
        checks.register::<regia_scene::prefab::PendingPrefab>("PendingPrefab", check_pending_prefab);
        checks
    }
}

impl ConfigChecks {
    /// Registers `check` for entities holding a `T` (named `component` in listings); the check
    /// re-runs when a `T` is added or changed on the entity or one of its children.
    pub fn register<T: Component>(&mut self, component: &'static str, check: CheckFn) {
        self.watch::<T>();
        self.checks.push(Registered {
            component,
            type_id: TypeId::of::<T>(),
            check,
        });
        self.since = None;
    }

    /// Re-runs the checks of an entity when a `T` on it (or a child) is added or changed, without
    /// registering a check for `T`.
    pub fn watch<T: Component>(&mut self) {
        if !self.watched.iter().any(|(id, _)| *id == TypeId::of::<T>()) {
            self.watched.push((TypeId::of::<T>(), changed_entities::<T>));
        }
    }

    /// The registered checks' component names, in registration order.
    #[must_use]
    pub fn components(&self) -> Vec<&'static str> {
        self.checks.iter().map(|c| c.component).collect()
    }

    /// The warnings of `entity` (empty when it has none).
    #[must_use]
    pub fn warnings_of(&self, entity: Entity) -> &[ConfigWarning] {
        self.warnings.get(&entity).map_or(&[], Vec::as_slice)
    }

    /// Every warned entity and its warnings, by entity id.
    #[must_use]
    pub fn all(&self) -> &BTreeMap<Entity, Vec<ConfigWarning>> {
        &self.warnings
    }

    /// Whether a descendant of `entity` is warned (its row gets the dimmed badge).
    #[must_use]
    pub fn descendant_warned(&self, entity: Entity) -> bool {
        self.ancestors.contains(&entity)
    }

    /// How many entity evaluations ran so far (a counter for tests: an idle update adds none).
    #[must_use]
    pub fn evaluations(&self) -> u64 {
        self.evaluations
    }

    /// How many updates evaluated at least one entity.
    #[must_use]
    pub fn passes(&self) -> u64 {
        self.passes
    }

    /// Bumped whenever the set of warnings changed.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Runs every check of `entity` now (no caching).
    #[must_use]
    pub fn evaluate(&self, world: &World, entity: Entity) -> Vec<ConfigWarning> {
        let Ok(entity_ref) = world.get_entity(entity) else {
            return Vec::new();
        };
        self.checks
            .iter()
            .filter(|c| {
                world
                    .components()
                    .get_id(c.type_id)
                    .is_some_and(|id| entity_ref.contains_id(id))
            })
            .filter_map(|c| (c.check)(world, entity))
            .collect()
    }
}

/// The entities whose `T` was added or changed after `since`.
fn changed_entities<T: Component>(world: &mut World, since: Tick) -> Vec<Entity> {
    use regia_ecs::ecs::change_detection::{DetectChanges as _, Ref};
    let now = world.change_tick();
    let mut query = world.query::<(Entity, Ref<T>)>();
    query
        .iter(world)
        .filter(|(_, value)| value.last_changed().is_newer_than(since, now))
        .map(|(entity, _)| entity)
        .collect()
}

/// The archetype signature (ids and sizes, resources left out): a spawn, despawn, insert or
/// removal changes it.
fn structure_signature(world: &World) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let resource = world.component_id::<regia_ecs::ecs::resource::IsResource>();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for archetype in world.archetypes().iter() {
        if resource.is_some_and(|id| archetype.contains(id)) {
            continue;
        }
        archetype.id().hash(&mut hasher);
        archetype.len().hash(&mut hasher);
    }
    hasher.finish()
}

/// The entities holding any watched component, with their archetypes.
fn watched_entities(world: &World, watched: &[(TypeId, ChangedFn)]) -> Vec<(Entity, u32)> {
    let ids: Vec<_> = watched
        .iter()
        .filter_map(|(type_id, _)| world.components().get_id(*type_id))
        .collect();
    world
        .iter_entities()
        .filter(|e| ids.iter().any(|id| e.contains_id(*id)))
        .map(|e| (e.id(), e.archetype().id().index() as u32))
        .collect()
}

/// Brings the warnings up to date: re-runs the checks of the entities whose watched components
/// changed since the last update (and of their parents, for checks that read children). After a
/// structural change (a spawn, a despawn, a component added or removed anywhere) only the watched
/// entities whose archetype moved, and the warned ones that left, are re-checked. An update with
/// no change evaluates nothing. Returns how many entities were evaluated.
pub fn update(world: &mut World) -> usize {
    let mut checks = world.remove_resource::<ConfigChecks>().unwrap_or_default();
    let evaluated = update_with(world, &mut checks);
    world.insert_resource(checks);
    evaluated
}

fn update_with(world: &mut World, checks: &mut ConfigChecks) -> usize {
    let signature = structure_signature(world);
    let mut dirty: Vec<Entity> = Vec::new();
    if checks.signature != Some(signature) || checks.since.is_none() {
        let now = watched_entities(world, &checks.watched);
        let fresh = checks.since.is_none();
        let mut placed = std::collections::HashMap::with_capacity(now.len());
        for (entity, archetype) in now {
            if fresh || checks.archetypes.get(&entity) != Some(&archetype) {
                dirty.push(entity);
            }
            placed.insert(entity, archetype);
        }
        // Entities that left the watched set (despawned, or lost their last watched component).
        dirty.extend(
            checks
                .archetypes
                .keys()
                .filter(|e| !placed.contains_key(e))
                .copied(),
        );
        checks.archetypes = placed;
    }
    if let Some(since) = checks.since {
        for (_, finder) in checks.watched.clone() {
            dirty.extend(finder(world, since));
        }
    }
    checks.signature = Some(signature);
    checks.since = Some(world.increment_change_tick());
    if dirty.is_empty() {
        return 0;
    }
    // Parents too: a body's check reads its children's colliders.
    let parents: Vec<Entity> = dirty
        .iter()
        .filter_map(|e| world.get::<ChildOf>(*e).map(ChildOf::parent))
        .collect();
    dirty.extend(parents);
    dirty.sort_unstable();
    dirty.dedup();
    let _span =
        tracing::trace_span!(target: "regia_editor", "config_checks", entities = dirty.len()).entered();
    let mut changed = false;
    for entity in &dirty {
        checks.evaluations += 1;
        let found = checks.evaluate(world, *entity);
        let previous = checks.warnings.get(entity);
        if found.is_empty() {
            changed |= checks.warnings.remove(entity).is_some();
        } else if previous != Some(&found) {
            checks.warnings.insert(*entity, found);
            changed = true;
        }
    }
    checks
        .warnings
        .retain(|entity, _| world.get_entity(*entity).is_ok());
    checks.passes += 1;
    // Ancestors: a reparent moves the dimmed badges without changing any warning.
    let mut ancestors = HashSet::new();
    for entity in checks.warnings.keys() {
        let mut seen = HashSet::new();
        let mut cursor = world.get::<ChildOf>(*entity).map(ChildOf::parent);
        while let Some(parent) = cursor {
            if !seen.insert(parent) {
                break;
            }
            ancestors.insert(parent);
            cursor = world.get::<ChildOf>(parent).map(ChildOf::parent);
        }
    }
    if changed || ancestors != checks.ancestors {
        checks.ancestors = ancestors;
        checks.revision += 1;
    }
    dirty.len()
}

// ---------------------------------------------------------------------------------------------
// Seed checks
// ---------------------------------------------------------------------------------------------

fn has_collider_in_subtree(world: &World, entity: Entity) -> bool {
    let mut stack = vec![entity];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        if world.get::<regia_physics::Collider>(current).is_some() {
            return true;
        }
        // A child body is its own body: its colliders are not this body's.
        if current != entity && world.get::<regia_physics::RigidBody>(current).is_some() {
            continue;
        }
        if let Some(children) = world.get::<Children>(current) {
            stack.extend(children.iter());
        }
    }
    false
}

fn check_rigid_body(world: &World, entity: Entity) -> Option<ConfigWarning> {
    (!has_collider_in_subtree(world, entity)).then(|| {
        ConfigWarning::new(
            "rigid_body_without_collider",
            "This rigid body has no collider, so it collides with nothing.",
            "Add a Collider to this entity or to a child without its own rigid body.",
        )
    })
}

/// Whether a collider shape has no volume.
#[must_use]
pub fn shape_is_empty(shape: &regia_physics::ColliderShape) -> bool {
    use regia_physics::ColliderShape as S;
    let flat = |v: f32| v.is_nan() || v <= 0.0;
    match shape {
        S::Box { half_extents } => flat(half_extents.x) || flat(half_extents.y) || flat(half_extents.z),
        S::Sphere { radius } => flat(*radius),
        S::Capsule { radius, .. } => flat(*radius),
        S::Cylinder {
            half_height, radius, ..
        }
        | S::Cone {
            half_height, radius, ..
        } => flat(*half_height) || flat(*radius),
        S::ConvexHull { points } => points.len() < 4,
        S::TriMesh { vertices, indices } => vertices.is_empty() || indices.is_empty(),
        S::HeightField { nx, nz, heights, .. } => *nx < 2 || *nz < 2 || heights.is_empty(),
        S::Compound { shapes } => shapes.is_empty() || shapes.iter().all(|(_, s)| shape_is_empty(s)),
    }
}

fn check_collider(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let collider = world.get::<regia_physics::Collider>(entity)?;
    shape_is_empty(&collider.shape).then(|| {
        ConfigWarning::new(
            "collider_zero_size",
            "This collider's shape has zero size (or too few points), so it never touches anything.",
            "Give the shape a positive size in the inspector.",
        )
    })
}

fn not_positive(v: f32) -> bool {
    v.is_nan() || v <= 0.0
}

fn light_warning(intensity: bool, range: bool) -> Option<ConfigWarning> {
    match (intensity, range) {
        (true, _) => Some(ConfigWarning::new(
            "light_zero_intensity",
            "This light has zero intensity, so it lights nothing.",
            "Raise the light's intensity above zero.",
        )),
        (false, true) => Some(ConfigWarning::new(
            "light_zero_range",
            "This light has zero range, so it reaches nothing.",
            "Raise the light's range (radius) above zero.",
        )),
        _ => None,
    }
}

fn check_point_light(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let light = world.get::<regia_render::PointLight>(entity)?;
    light_warning(not_positive(light.intensity), not_positive(light.radius))
}

fn check_spot_light(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let light = world.get::<regia_render::SpotLight>(entity)?;
    light_warning(not_positive(light.intensity), not_positive(light.range))
}

// Rect and tube lights take a range of 0 as "automatic" (the fitted emitters' cutoff).
fn check_rect_light(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let light = world.get::<regia_render::RectLight>(entity)?;
    light_warning(
        not_positive(light.intensity),
        light.range.is_nan() || light.range < 0.0,
    )
}

fn check_tube_light(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let light = world.get::<regia_render::TubeLight>(entity)?;
    light_warning(
        not_positive(light.intensity),
        light.range.is_nan() || light.range < 0.0,
    )
}

fn check_directional_light(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let light = world.get::<regia_render::DirectionalLight>(entity)?;
    light_warning(not_positive(light.illuminance), false)
}

fn check_audio_emitter(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let emitter = world.get::<regia_audio::AudioEmitter>(entity)?;
    let silent = emitter.sound.is_none()
        && emitter.cue.is_none()
        && emitter.sound_name.trim().is_empty()
        && emitter.cue_name.trim().is_empty();
    silent.then(|| {
        ConfigWarning::new(
            "audio_emitter_without_sound",
            "This audio emitter has no sound or cue, so it plays nothing.",
            "Pick a sound (sound_name) or a cue (cue_name) in the inspector.",
        )
    })
}

fn check_camera(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let camera = world.get::<regia_render::Camera>(entity)?;
    // NaN compares false: a NaN plane is reported too.
    (camera.near.partial_cmp(&camera.far) != Some(std::cmp::Ordering::Less)).then(|| {
        ConfigWarning::new(
            "camera_near_not_before_far",
            format!(
                "This camera's near plane ({}) is not in front of its far plane ({}).",
                camera.near, camera.far
            ),
            "Set near below far (for example near 0.1, far 1000).",
        )
    })
}

fn check_mesh_source(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let source = world.get::<regia_render::MeshSource>(entity)?;
    source.mesh.trim().is_empty().then(|| {
        ConfigWarning::new(
            "mesh_without_mesh",
            "This mesh source names no mesh, so nothing is drawn.",
            "Pick a mesh asset (or a built-in shape) in the inspector.",
        )
    })
}

fn missing_prefab(prefab: u128) -> ConfigWarning {
    ConfigWarning::new(
        "prefab_source_missing",
        format!(
            "The source prefab of this instance ({}) is not loaded.",
            regia_core::id::Guid::from_u128(prefab)
        ),
        "Restore or re-import the prefab file, or break the instance (Break prefab).",
    )
}

fn check_prefab_source(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let instance = world.get::<regia_scene::prefab::PrefabInstance>(entity)?;
    let library = world.get_resource::<regia_scene::prefab::PrefabLibrary>()?;
    (!library.contains(instance.prefab_guid())).then(|| missing_prefab(instance.prefab))
}

fn check_pending_prefab(world: &World, entity: Entity) -> Option<ConfigWarning> {
    let pending = world.get::<regia_scene::prefab::PendingPrefab>(entity)?;
    // Still waiting for its prefab: missing while the library does not have it.
    let library = world.get_resource::<regia_scene::prefab::PrefabLibrary>();
    (!library.is_some_and(|l| l.contains(regia_core::id::Guid::from_u128(pending.prefab))))
        .then(|| missing_prefab(pending.prefab))
}

// ---------------------------------------------------------------------------------------------
// Agent surface
// ---------------------------------------------------------------------------------------------

fn warning_json(w: &ConfigWarning) -> serde_json::Value {
    serde_json::json!({ "code": w.code, "text": w.text, "fix_hint": w.fix_hint })
}

/// `editor.config_warnings`: brings the warnings up to date and lists them (one entity's, or
/// every warned entity's).
fn config_warnings_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::RpcError;
    use serde_json::{Value, json};
    let entity = match params.get("entity") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(crate::command::entity_from_bits)
                .ok_or_else(|| RpcError::wrong_type("entity", "an entity id"))?,
        ),
    };
    if let Some(entity) = entity
        && world.get_entity(entity).is_err()
    {
        return Err(RpcError::invalid_params(format!(
            "no_such_entity: entity {} does not exist",
            entity.to_bits()
        )));
    }
    let evaluated = update(world);
    let checks = world.get_resource::<ConfigChecks>().cloned().unwrap_or_default();
    let entry = |entity: Entity, warnings: &[ConfigWarning]| {
        json!({
            "entity": entity.to_bits(),
            "name": crate::outliner::display_name(world, entity),
            "warnings": warnings.iter().map(warning_json).collect::<Vec<_>>(),
            "descendant_warned": checks.descendant_warned(entity),
        })
    };
    let entities: Vec<Value> = match entity {
        Some(entity) => vec![entry(entity, checks.warnings_of(entity))],
        None => checks.all().iter().map(|(e, w)| entry(*e, w)).collect(),
    };
    Ok(json!({
        "entities": entities,
        "warned": checks.all().len(),
        "evaluated": evaluated,
        "evaluations": checks.evaluations(),
        "checks": checks.components(),
    }))
}

/// The configuration-warning surface (`editor.config_warnings`).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.config_warnings",
            tool: "config_warnings",
            description: "Configuration warnings (a rigid body without a collider, a zero-size collider, a light \
                with zero intensity or range, a silent audio emitter, a camera with near >= far, a mesh source \
                without a mesh, a prefab instance whose prefab is missing): re-runs the checks of the entities \
                that changed since the last call and lists the warnings of one entity or of every warned \
                entity, each with a code, a text and a fix hint. Edits nothing.",
            params: &[ParamSchema {
                name: "entity",
                kind: "integer",
                required: false,
                description: "Entity id; absent = every warned entity.",
            }],
            result: "{ entities: [{ entity, name, warnings: [{ code, text, fix_hint }], descendant_warned }], \
                warned, evaluated, evaluations, checks }",
            mutating: false,
            permission: Permission::Read,
        },
        config_warnings_method as crate::viewport_agent::Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::Name;

    fn codes(checks: &ConfigChecks, entity: Entity) -> Vec<&'static str> {
        checks.warnings_of(entity).iter().map(|w| w.code).collect()
    }

    #[test]
    fn seed_checks_flag_each_misconfiguration_and_pass_the_fixed_one() {
        let mut world = World::new();
        let body = world
            .spawn((Name::new("Body"), regia_physics::RigidBody::dynamic()))
            .id();
        let empty_box = world
            .spawn(regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: regia_core::math::Vec3::new(0.5, 0.0, 0.5),
                },
                ..Default::default()
            })
            .id();
        let dark = world
            .spawn(regia_render::PointLight {
                position: [0.0; 3],
                color: [1.0; 3],
                intensity: 0.0,
                radius: 5.0,
                cast_shadows: false,
            })
            .id();
        let short = world
            .spawn(regia_render::PointLight {
                position: [0.0; 3],
                color: [1.0; 3],
                intensity: 800.0,
                radius: 0.0,
                cast_shadows: false,
            })
            .id();
        let silent = world.spawn(regia_audio::AudioEmitter::default()).id();
        let camera = world
            .spawn(regia_render::Camera {
                near: 10.0,
                far: 10.0,
                ..Default::default()
            })
            .id();
        let meshless = world.spawn(regia_render::MeshSource::default()).id();
        let orphan = world
            .spawn(regia_scene::prefab::PrefabInstance {
                prefab: 0xabc,
                ..Default::default()
            })
            .id();
        world.insert_resource(regia_scene::prefab::PrefabLibrary::new());
        let fine = world
            .spawn((
                regia_render::Camera::default(),
                regia_render::MeshSource::new(regia_render::MeshSource::BUILTIN_CUBE, ""),
            ))
            .id();
        update(&mut world);
        let checks = world.resource::<ConfigChecks>();
        assert_eq!(codes(checks, body), ["rigid_body_without_collider"]);
        assert_eq!(codes(checks, empty_box), ["collider_zero_size"]);
        assert_eq!(codes(checks, dark), ["light_zero_intensity"]);
        assert_eq!(codes(checks, short), ["light_zero_range"]);
        assert_eq!(codes(checks, silent), ["audio_emitter_without_sound"]);
        assert_eq!(codes(checks, camera), ["camera_near_not_before_far"]);
        assert_eq!(codes(checks, meshless), ["mesh_without_mesh"]);
        assert_eq!(codes(checks, orphan), ["prefab_source_missing"]);
        assert!(codes(checks, fine).is_empty());
        for warning in checks.all().values().flatten() {
            assert!(!warning.fix_hint.is_empty() && !warning.text.is_empty());
            assert!(
                warning
                    .code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                warning.code
            );
        }
    }

    #[test]
    fn checks_run_on_change_only_and_a_child_collider_clears_the_body() {
        let mut world = World::new();
        let body = world
            .spawn((Name::new("Crate"), regia_physics::RigidBody::dynamic()))
            .id();
        let other = world
            .spawn(regia_render::PointLight {
                position: [0.0; 3],
                color: [1.0; 3],
                intensity: 1.0,
                radius: 1.0,
                cast_shadows: false,
            })
            .id();
        update(&mut world);
        let after_first = world.resource::<ConfigChecks>().evaluations();
        assert!(after_first >= 2);
        // Idle updates evaluate nothing.
        for _ in 0..5 {
            assert_eq!(update(&mut world), 0);
        }
        assert_eq!(world.resource::<ConfigChecks>().evaluations(), after_first);
        // A value change re-runs only that entity.
        if let Some(mut light) = world.get_mut::<regia_render::PointLight>(other) {
            light.intensity = 0.0;
        }
        assert_eq!(update(&mut world), 1);
        assert_eq!(
            codes(world.resource::<ConfigChecks>(), other),
            ["light_zero_intensity"]
        );
        // A collider on a child clears the body's warning (the parent is re-checked).
        let child = world.spawn(regia_physics::Collider::default()).id();
        world.entity_mut(child).insert(ChildOf(body));
        update(&mut world);
        let checks = world.resource::<ConfigChecks>();
        assert!(codes(checks, body).is_empty(), "{:?}", checks.warnings_of(body));
        // An ancestor of a warned entity gets the dimmed badge.
        let parent = world.spawn(Name::new("Lights")).id();
        world.entity_mut(other).insert(ChildOf(parent));
        update(&mut world);
        let checks = world.resource::<ConfigChecks>();
        assert!(checks.descendant_warned(parent));
        assert!(!checks.descendant_warned(other));
        let revision = checks.revision();
        // Fixing it clears both badges and bumps the revision.
        if let Some(mut light) = world.get_mut::<regia_render::PointLight>(other) {
            light.intensity = 5.0;
        }
        update(&mut world);
        let checks = world.resource::<ConfigChecks>();
        assert!(checks.all().is_empty(), "{:?}", checks.all());
        assert!(!checks.descendant_warned(parent));
        assert!(checks.revision() > revision);
    }

    #[test]
    fn a_registered_check_runs_for_its_component() {
        #[derive(Component)]
        struct Fuel(f32);
        fn empty_tank(world: &World, entity: Entity) -> Option<ConfigWarning> {
            (world.get::<Fuel>(entity)?.0 <= 0.0)
                .then(|| ConfigWarning::new("tank_empty", "Empty.", "Refuel."))
        }
        let mut world = World::new();
        let mut checks = ConfigChecks::default();
        checks.register::<Fuel>("Fuel", empty_tank);
        world.insert_resource(checks);
        let tank = world.spawn(Fuel(0.0)).id();
        update(&mut world);
        assert_eq!(codes(world.resource::<ConfigChecks>(), tank), ["tank_empty"]);
        assert!(world.resource::<ConfigChecks>().components().contains(&"Fuel"));
    }
}
