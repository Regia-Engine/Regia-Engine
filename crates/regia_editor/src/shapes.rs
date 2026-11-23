// SPDX-License-Identifier: MPL-2.0
//! The Create menu: greybox primitives (Create ▸ Shape, `regia_render::Primitive`) and the
//! placeable actors (Create ▸ Player start, Trigger volume, Point light, Spot light, Directional
//! light, Camera, Reflection capture, Audio emitter, Rect light, Tube light, Decal, Post-process
//! volume, Spline, Nav link, Smart object; ADR-7468, ADR-7592, ADR-7741).
//!
//! [`create_shape`] and [`create_actor`] spawn one default entity at the viewport's orbit target
//! (its focus point) through the undoable [`SpawnEntities`] command (so it saves with the level and
//! undoes like any create, one undo step), and select it so the inspector shows its parameters.
//! `editor.create_entity { kind }` is the same operation for agents, for every entry of the menu
//! ([`create_kind`]); `scene.spawn_primitive` predates it for shapes.

use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_render::{Primitive, PrimitiveShape};
use regia_scene::prelude::{EntityGuid, Transform};

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use serde_json::{Value, json};

use crate::agent_parity::EditorAction;
use crate::command::{EditorCommand, SpawnEntities, UndoStack};
use crate::selection::{SelectMode, Selection};
use crate::viewport::EditorCamera;

/// Menu label of a shape kind (`"conical_frustum"` → `"Conical frustum"`).
#[must_use]
pub fn shape_label(kind: &str) -> String {
    let mut label = kind.replace('_', " ");
    if let Some(first) = label.get(0..1) {
        let upper = first.to_uppercase();
        label.replace_range(0..1, &upper);
    }
    label
}

/// Spawns a default `kind` shape (one of [`PrimitiveShape::KINDS`]) at the editor camera's
/// target, pushes the undo entry and selects the new entity.
///
/// # Errors
/// An unknown kind, or a reflection failure building the spawn command.
pub fn create_shape(world: &mut World, kind: &str) -> Result<Entity, String> {
    create_shape_at(world, kind, None)
}

/// [`create_shape`] at `at` instead of the camera target when given.
///
/// # Errors
/// As [`create_shape`].
pub fn create_shape_at(world: &mut World, kind: &str, at: Option<Vec3>) -> Result<Entity, String> {
    let shape = PrimitiveShape::default_of(kind).map_err(|e| e.to_string())?;
    let translation = at.unwrap_or_else(|| {
        world
            .get_resource::<EditorCamera>()
            .map_or(Vec3::ZERO, |c| c.target)
    });
    // Register on demand: an edit world built without the render plugin still saves shapes.
    world
        .resource::<AppTypeRegistry>()
        .clone()
        .write()
        .register::<Primitive>();
    let guid = Guid::new_random();
    let mut command = SpawnEntities::new_entity(
        world,
        guid,
        &shape_label(kind),
        Transform::from_translation(translation),
    )
    .and_then(|command| command.with_component(world, &Primitive::new(shape)))
    .map_err(|e| e.to_string())?;
    command.apply(world);
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(command));
    let entity = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
        .ok_or_else(|| format!("{kind} was not spawned"))?;
    world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .select(entity, SelectMode::Replace);
    Ok(entity)
}

/// The Create menu's actor entries, in menu order. Each action's id is `create.<kind>`, the
/// `kind` of `editor.create_entity` ([`actor_kind`]).
pub const ACTOR_ENTRIES: [&EditorAction; 15] = [
    &crate::agent_parity::CREATE_PLAYER_START,
    &crate::agent_parity::CREATE_TRIGGER_VOLUME,
    &crate::agent_parity::CREATE_POINT_LIGHT,
    &crate::agent_parity::CREATE_SPOT_LIGHT,
    &crate::agent_parity::CREATE_DIRECTIONAL_LIGHT,
    &crate::agent_parity::CREATE_CAMERA,
    &crate::agent_parity::CREATE_REFLECTION_CAPTURE,
    &crate::agent_parity::CREATE_AUDIO_EMITTER,
    &crate::agent_parity::CREATE_RECT_LIGHT,
    &crate::agent_parity::CREATE_TUBE_LIGHT,
    &crate::agent_parity::CREATE_DECAL,
    &crate::agent_parity::CREATE_POST_PROCESS_VOLUME,
    &crate::agent_parity::CREATE_SPLINE,
    &crate::agent_parity::CREATE_NAV_LINK,
    &crate::agent_parity::CREATE_SMART_OBJECT,
];

/// The `editor.create_entity` kind of an actor entry (`create.player_start` → `player_start`).
#[must_use]
pub fn actor_kind(action: &EditorAction) -> &'static str {
    action.id.strip_prefix("create.").unwrap_or(action.id)
}

/// Half extent of a new trigger volume's box, meters (a 2 m cube).
pub const TRIGGER_HALF_EXTENT: f32 = 1.0;

/// The built-in decal prefab a new Create ▸ Decal names (an empty name draws nothing).
pub const DECAL_PREFAB: &str = "scorch";

/// Half length of a new spline's two points' span, meters (the points sit at local X ±2).
pub const SPLINE_HALF_SPAN: f32 = 2.0;

/// Half extent of a new reflection capture's parallax box, meters (a 4 m cube around the focus
/// point; the blend margin is the component's default, a quarter of it).
pub const CAPTURE_HALF_EXTENT: f32 = 2.0;

/// The id a new reflection capture takes: one past the largest `LocalCapture` id in `world`, so
/// it never moves another capture's slot or `.fcap` entry (ids start at 1; 0 is the component
/// default a hand-added capture has).
#[must_use]
pub fn next_capture_id(world: &World) -> u64 {
    world
        .iter_entities()
        .filter_map(|entity| entity.get::<regia_render::LocalCapture>().map(|c| c.id))
        .max()
        .map_or(1, |max| max.saturating_add(1).max(1))
}

/// Gives the reflection captures among copied scene `records` (instances' nested
/// `added_children` included) ids no other capture uses, before the copy point spawns them
/// (Edit ▸ Duplicate, Copy/Paste, the gizmo's Alt-drag duplicate and the Outliner's Duplicate;
/// ADR-7663). A copied capture keeps its id only when no live capture of `world` and no earlier
/// record of the copy uses it (a Cut then Paste moves a capture and keeps its bake); any other
/// takes the next free id, one past the largest in use, in file order. The rewrite is in the
/// copy's scene file, so undo removes the copy and redo brings it back with the same new id;
/// the original keeps its own. Returns the `(old, new)` ids changed.
///
/// A record whose capture does not decode is left as it is (and logged): the bake's
/// `capture_duplicate_id` still names it.
pub fn fresh_capture_ids(
    world: &World,
    records: &mut [regia_scene::prelude::SceneEntity],
) -> Vec<(u64, u64)> {
    use regia_ecs::reflect::TypePath as _;
    let key = regia_render::LocalCapture::type_path();
    if !records_have(records, key) {
        return Vec::new();
    }
    let registry = world.resource::<AppTypeRegistry>().clone();
    let registry = registry.read();
    let Some(registration) = registry.get_with_type_path(key) else {
        tracing::warn!(target: "regia_editor", "LocalCapture is not registered: copied capture ids kept");
        return Vec::new();
    };
    let used: std::collections::BTreeSet<u64> = world
        .iter_entities()
        .filter_map(|entity| entity.get::<regia_render::LocalCapture>().map(|c| c.id))
        .collect();
    let mut renumber = Renumber {
        codec: regia_ecs::serialize::ReflectCodec::new(&registry),
        registration,
        key,
        library: world.get_resource::<regia_scene::prefab::PrefabLibrary>(),
        next: next_capture_id(world),
        used,
        changed: Vec::new(),
    };
    renumber.records(records);
    renumber.changed
}

/// Whether any of `records` (nested added children included) carries the component `key`, or
/// instances a prefab (whose template may hold a capture).
fn records_have(records: &[regia_scene::prelude::SceneEntity], key: &str) -> bool {
    records.iter().any(|record| {
        record.prefab_instance.is_some() || record.components.keys().any(|k| capture_key(k, key))
    })
}

/// The field path of the id override an instance records for a capture it holds.
const CAPTURE_ID_PATH: &str = ".id";

/// An override path names the capture's `id` field (`.id` or `id`).
fn is_id_path(path: &str) -> bool {
    path.strip_prefix('.').unwrap_or(path) == "id"
}

/// The entities under `root` (root included), in tree order.
fn subtree_of(world: &World, root: Entity) -> Vec<Entity> {
    let mut members = Vec::new();
    let mut stack = vec![root];
    while let Some(entity) = stack.pop() {
        members.push(entity);
        if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter().rev());
        }
    }
    members
}

/// The instance record a prefab-internal `entity` saves into: the outermost ancestor that
/// carries `PrefabInstance` and is not itself prefab-internal (a nested instance is part of
/// its outer template, which owns the saved record).
fn outermost_instance(world: &World, entity: Entity) -> Option<Entity> {
    let mut found = None;
    let mut cursor = entity;
    for _ in 0..64 {
        let Some(parent) = world.get::<ChildOf>(cursor).map(ChildOf::parent) else {
            break;
        };
        if world.get::<regia_scene::prefab::PrefabInstance>(parent).is_some()
            && world.get::<regia_scene::prefab::PrefabLink>(parent).is_none()
        {
            found = Some(parent);
        }
        cursor = parent;
    }
    found
}

/// Gives the reflection captures a freshly instanced prefab brought (`root`'s subtree) ids no
/// other capture uses (ADR-7716, closing the known issue of ADR-7699). Every instance of a prefab
/// whose template holds a capture starts with the template's id, so two instances would share
/// one and the bake would refuse with `capture_duplicate_id`. A capture keeps its id when no
/// capture outside the instance (and no earlier one in it) uses it; any other takes the next free
/// id, one past the largest in use. The new id is set on the live capture and recorded as an
/// override (`.id` on the template's entity) on the instance, so it saves with the level and a
/// reload, an undo and a redo give the same id. Returns the `(old, new)` ids changed.
///
/// A capture inside a prefab nested twice in one template shares its local id with its twin:
/// their overrides cannot be told apart, so both keep the shared id (the bake still names it).
pub fn renumber_instance_captures(world: &mut World, root: Entity) -> Vec<(u64, u64)> {
    use regia_ecs::reflect::TypePath as _;
    let members = subtree_of(world, root);
    let inside: std::collections::BTreeSet<Entity> = members.iter().copied().collect();
    let mut used: std::collections::BTreeSet<u64> = world
        .iter_entities()
        .filter(|entity| !inside.contains(&entity.id()))
        .filter_map(|entity| entity.get::<regia_render::LocalCapture>().map(|c| c.id))
        .collect();
    let mut next = next_capture_id(world);
    let mut changed = Vec::new();
    for &member in &members {
        let Some(id) = world.get::<regia_render::LocalCapture>(member).map(|c| c.id) else {
            continue;
        };
        let Some(link) = world.get::<regia_scene::prefab::PrefabLink>(member).copied() else {
            continue;
        };
        if used.insert(id) {
            continue;
        }
        // A twin (the same local id twice: a prefab nested twice) shares its override.
        let twins = members
            .iter()
            .filter(|other| {
                world.get::<regia_render::LocalCapture>(**other).is_some()
                    && world
                        .get::<regia_scene::prefab::PrefabLink>(**other)
                        .is_some_and(|l| l.local_id == link.local_id)
            })
            .count();
        let Some(instance_root) = outermost_instance(world, member).filter(|_| twins == 1) else {
            continue;
        };
        while used.contains(&next) {
            next = next.saturating_add(1);
        }
        let new = next;
        used.insert(new);
        if let Some(mut capture) = world.get_mut::<regia_render::LocalCapture>(member) {
            capture.id = new;
        }
        if let Some(mut instance) = world.get_mut::<regia_scene::prefab::PrefabInstance>(instance_root) {
            let key = regia_render::LocalCapture::type_path();
            if let Some(existing) = instance.overrides.iter_mut().find(|entry| {
                entry.target == link.local_id && capture_key(&entry.component, key) && is_id_path(&entry.path)
            }) {
                existing.value = new.to_string();
            } else {
                instance.overrides.push(regia_scene::prefab::OverrideEntry {
                    target: link.local_id,
                    component: key.to_string(),
                    path: CAPTURE_ID_PATH.to_string(),
                    value: new.to_string(),
                });
            }
        }
        changed.push((id, new));
    }
    changed
}

/// A record's component key names `LocalCapture` (a file written before the brand rename names
/// the legacy crate).
fn capture_key(record_key: &str, key: &str) -> bool {
    record_key == key || regia_core::brand::current_type_path(record_key) == key
}

/// The state of one [`fresh_capture_ids`] pass.
struct Renumber<'r> {
    codec: regia_ecs::serialize::ReflectCodec<'r>,
    registration: &'r regia_ecs::reflect::TypeRegistration,
    key: &'static str,
    /// The baked templates, which hold the ids a copied instance's captures start with.
    library: Option<&'r regia_scene::prefab::PrefabLibrary>,
    /// The next id to try for a copy that needs one.
    next: u64,
    /// Ids taken: the live captures' and the copies' so far.
    used: std::collections::BTreeSet<u64>,
    changed: Vec<(u64, u64)>,
}

impl Renumber<'_> {
    fn records(&mut self, records: &mut [regia_scene::prelude::SceneEntity]) {
        for record in records {
            let keys: Vec<String> = record
                .components
                .keys()
                .filter(|k| capture_key(k, self.key))
                .cloned()
                .collect();
            for k in keys {
                if let Some(raw) = record.components.get_mut(&k) {
                    self.one(raw);
                }
            }
            if let Some(instance) = &mut record.prefab_instance {
                self.instance(instance);
                self.records(&mut instance.added_children);
            }
        }
    }

    /// The captures a copied instance's template holds (and the ones its overrides set): the id
    /// each ends up with is the override's, else the template's. One that collides takes a new
    /// id, written as an `.id` override on the instance record (ADR-7716).
    fn instance(&mut self, instance: &mut regia_scene::prelude::PrefabInstanceData) {
        use regia_ecs::reflect::FromReflect as _;
        use regia_scene::prefab::BakedValue;
        // (local id, the id the template gives it) of every capture the template holds.
        let mut targets: Vec<(Guid, Option<u64>)> = Vec::new();
        if let Some(template) = self.library.and_then(|library| library.get(instance.prefab.0)) {
            for entity in &template.entities {
                for (component, baked) in &entity.components {
                    if !capture_key(component, self.key) {
                        continue;
                    }
                    let id = match baked {
                        BakedValue::Reflected(value) => {
                            regia_render::LocalCapture::from_reflect(value.as_partial_reflect()).map(|c| c.id)
                        }
                        BakedValue::Raw(..) => None,
                    };
                    targets.push((entity.local_id, id));
                }
            }
        }
        for entry in &instance.overrides {
            if capture_key(&entry.component, self.key)
                && is_id_path(&entry.path)
                && !targets.iter().any(|(local, _)| *local == entry.target)
            {
                targets.push((entry.target, None));
            }
        }
        for (local, template_id) in targets {
            let at = instance.overrides.iter().position(|entry| {
                entry.target == local && capture_key(&entry.component, self.key) && is_id_path(&entry.path)
            });
            let set = at.and_then(|at| instance.overrides[at].value.get_ron().trim().parse::<u64>().ok());
            let Some(id) = set.or(template_id) else {
                continue;
            };
            if self.used.insert(id) {
                continue;
            }
            while self.used.contains(&self.next) {
                self.next = self.next.saturating_add(1);
            }
            let new = self.next;
            let Some(value) = ron::value::RawValue::from_ron(&new.to_string())
                .ok()
                .map(|raw| raw.to_owned())
            else {
                continue;
            };
            match at {
                Some(at) => instance.overrides[at].value = value,
                None => instance.overrides.push(regia_scene::prelude::PrefabOverride {
                    target: local,
                    component: self.key.to_string(),
                    path: CAPTURE_ID_PATH.to_string(),
                    value,
                }),
            }
            self.used.insert(new);
            self.changed.push((id, new));
        }
    }

    fn one(&mut self, raw: &mut Box<ron::value::RawValue>) {
        use regia_ecs::reflect::FromReflect as _;
        let decoded = self
            .codec
            .from_ron_typed(raw.get_ron(), self.registration)
            .ok()
            .and_then(|value| regia_render::LocalCapture::from_reflect(value.as_partial_reflect()));
        let Some(mut capture) = decoded else {
            tracing::warn!(target: "regia_editor", "a copied LocalCapture does not decode: its id is kept");
            return;
        };
        if self.used.insert(capture.id) {
            return;
        }
        while self.used.contains(&self.next) {
            self.next = self.next.saturating_add(1);
        }
        let old = capture.id;
        capture.id = self.next;
        let text = self.codec.to_ron_typed(capture.as_partial_reflect());
        let Some(new_raw) = text.ok().and_then(|text| {
            ron::value::RawValue::from_ron(text.trim_end())
                .ok()
                .map(|r| r.to_owned())
        }) else {
            tracing::warn!(target: "regia_editor", "a copied LocalCapture does not encode: its id is kept");
            return;
        };
        *raw = new_raw;
        self.used.insert(capture.id);
        self.changed.push((old, capture.id));
    }
}

/// The reflected component a new `kind` actor carries, and its rotation.
fn actor_component(
    world: &World,
    kind: &str,
) -> Result<(Box<dyn PartialReflect>, regia_core::math::Quat), String> {
    use regia_core::math::Quat;
    let capture_id = next_capture_id(world);
    let registry = world.resource::<AppTypeRegistry>().clone();
    let mut registry = registry.write();
    // Register on demand, like shapes: an edit world built without the owning plugin still saves
    // the entity.
    let value: Box<dyn PartialReflect> = match kind {
        "player_start" => {
            registry.register::<regia_framework::player::PlayerStart>();
            Box::new(regia_framework::player::PlayerStart)
        }
        "trigger_volume" => {
            registry.register::<regia_physics::Collider>();
            Box::new(regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::splat(TRIGGER_HALF_EXTENT),
                },
                sensor: true,
                active_events: regia_physics::ActiveEvents {
                    collision: true,
                    contact_force: false,
                },
                ..regia_physics::Collider::default()
            })
        }
        "point_light" => {
            registry.register::<regia_render::PointLight>();
            Box::new(regia_render::PointLight::default())
        }
        "spot_light" => {
            registry.register::<regia_render::SpotLight>();
            Box::new(regia_render::SpotLight::default())
        }
        "directional_light" => {
            registry.register::<regia_render::DirectionalLight>();
            Box::new(regia_render::DirectionalLight::default())
        }
        "camera" => {
            registry.register::<regia_render::Camera>();
            Box::new(regia_render::Camera::default())
        }
        "reflection_capture" => {
            registry.register::<regia_render::LocalCapture>();
            Box::new(regia_render::LocalCapture::new(
                capture_id,
                [CAPTURE_HALF_EXTENT; 3],
            ))
        }
        "audio_emitter" => {
            registry.register::<regia_audio::AudioEmitter>();
            Box::new(regia_audio::AudioEmitter::default())
        }
        "rect_light" => {
            registry.register::<regia_render::RectLight>();
            Box::new(regia_render::RectLight::default())
        }
        "tube_light" => {
            registry.register::<regia_render::TubeLight>();
            Box::new(regia_render::TubeLight::default())
        }
        "decal" => {
            registry.register::<regia_vfx::StaticDecal>();
            Box::new(regia_vfx::StaticDecal {
                prefab: DECAL_PREFAB.to_string(),
                ..regia_vfx::StaticDecal::default()
            })
        }
        "post_process_volume" => {
            registry.register::<regia_render::PostVolume>();
            Box::new(regia_render::PostVolume::default())
        }
        "spline" => {
            use regia_scene::spline::{Spline, SplinePoint};
            registry.register::<Spline>();
            registry.register::<SplinePoint>();
            // An empty spline draws nothing and has no curve to edit: two points, 4 m apart.
            let point = |x: f32| SplinePoint {
                position: Vec3::new(x, 0.0, 0.0),
                ..SplinePoint::default()
            };
            Box::new(Spline {
                points: vec![point(-SPLINE_HALF_SPAN), point(SPLINE_HALF_SPAN)],
                ..Spline::default()
            })
        }
        "nav_link" => {
            registry.register::<regia_nav::NavLinkAnchor>();
            Box::new(regia_nav::NavLinkAnchor::default())
        }
        "smart_object" => {
            registry.register::<regia_ai::SmartObject>();
            Box::new(regia_ai::SmartObject::default())
        }
        other => return Err(format!("unknown actor kind `{other}`")),
    };
    // A spot light points down its local -Z: aim a new one (and a rect light, which emits toward
    // its -Z) at the floor.
    let rotation = if matches!(kind, "spot_light" | "rect_light") {
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)
    } else {
        Quat::IDENTITY
    };
    Ok((value, rotation))
}

/// Spawns a default `kind` actor (one of [`ACTOR_ENTRIES`]: `player_start`, `trigger_volume`,
/// `point_light`, `spot_light`, `directional_light`, `camera`, `reflection_capture`,
/// `audio_emitter`, `rect_light`, `tube_light`, `decal`, `post_process_volume`, `spline`,
/// `nav_link` or `smart_object`) at `at` (default: the editor camera's target), pushes the undo
/// entry and selects the new entity.
///
/// # Errors
/// An unknown kind, or a reflection failure building the spawn command.
pub fn create_actor(world: &mut World, kind: &str, at: Option<Vec3>) -> Result<Entity, String> {
    let action = ACTOR_ENTRIES
        .iter()
        .find(|action| actor_kind(action) == kind)
        .ok_or_else(|| format!("unknown actor kind `{kind}`"))?;
    let (component, rotation) = actor_component(world, kind)?;
    let translation = at.unwrap_or_else(|| {
        world
            .get_resource::<EditorCamera>()
            .map_or(Vec3::ZERO, |c| c.target)
    });
    let guid = Guid::new_random();
    let command = SpawnEntities::new_entity(
        world,
        guid,
        action.label,
        Transform {
            translation,
            rotation,
            ..Transform::default()
        },
    )
    .and_then(|command| command.with_component(world, component.as_ref()))
    .map_err(|e| e.to_string())?;
    spawn_and_select(world, command, guid, kind)
}

/// Applies a create command, pushes it as one undo step and selects the new entity.
fn spawn_and_select(
    world: &mut World,
    mut command: SpawnEntities,
    guid: Guid,
    kind: &str,
) -> Result<Entity, String> {
    command.apply(world);
    crate::undo_agent::push_discrete(world, Box::new(command));
    let entity = world
        .query::<(Entity, &EntityGuid)>()
        .iter(world)
        .find(|(_, g)| g.0 == guid)
        .map(|(e, _)| e)
        .ok_or_else(|| format!("{kind} was not spawned"))?;
    world
        .get_resource_or_insert_with::<Selection>(Selection::default)
        .select(entity, SelectMode::Replace);
    Ok(entity)
}

/// Every `editor.create_entity` kind: the actors, then the shapes ([`PrimitiveShape::KINDS`]).
#[must_use]
pub fn create_kinds() -> Vec<&'static str> {
    ACTOR_ENTRIES
        .iter()
        .map(|action| actor_kind(action))
        .chain(PrimitiveShape::KINDS.iter().copied())
        .collect()
}

/// Creates any Create-menu entry by kind: an actor ([`create_actor`]) or a shape
/// ([`create_shape`], placed at `at` when given).
///
/// # Errors
/// An unknown kind, or a reflection failure building the spawn command.
pub fn create_kind(world: &mut World, kind: &str, at: Option<Vec3>) -> Result<Entity, String> {
    if ACTOR_ENTRIES.iter().any(|action| actor_kind(action) == kind) {
        return create_actor(world, kind, at);
    }
    create_shape_at(world, kind, at)
}

/// The Create ▸ Shape submenu (one button per kind) and the actor entries. Returns a toast
/// message when one was used.
pub fn shape_menu(ui: &mut egui::Ui, world: &mut World) -> Option<String> {
    let mut toast = None;
    crate::agent_parity::menu(ui, crate::agent_parity::CREATE_SHAPE.label, |ui| {
        for kind in PrimitiveShape::KINDS {
            // The feature profile may hide a kind (ADR-7753).
            if crate::feature_profiles::kind_hidden(world, kind) {
                continue;
            }
            // Each kind is `editor.command { run: "create.<kind>" }`.
            if crate::agent_parity::action_button_text(
                ui,
                &crate::agent_parity::CREATE_SHAPE,
                shape_label(kind),
            )
            .clicked()
            {
                toast = Some(match create_shape(world, kind) {
                    Ok(_) => format!("{} created", shape_label(kind)),
                    Err(err) => format!("Create failed: {err}"),
                });
                ui.close();
            }
        }
    });
    ui.separator();
    for action in ACTOR_ENTRIES {
        if crate::feature_profiles::kind_hidden(world, actor_kind(action)) {
            continue;
        }
        // Each entry is `editor.create_entity { kind: "<kind>" }`.
        if crate::agent_parity::action_menu_item(ui, action).clicked() {
            toast = Some(match create_actor(world, actor_kind(action), None) {
                Ok(_) => format!("{} created", action.label),
                Err(err) => format!("Create failed: {err}"),
            });
            ui.close();
        }
    }
    crate::agent_parity::menu(ui, "Blockout", |ui| {
        if let Some(message) = crate::blockout::menu(ui, world) {
            toast = Some(message);
        }
    });
    toast
}

/// `editor.create_entity { kind, translation? }`.
fn editor_create_entity(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let kinds = create_kinds();
    let kind = params.get("kind").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(format!("`kind` is required: one of {}", kinds.join(", ")))
    })?;
    if !kinds.contains(&kind) {
        let mut err = RpcError::invalid_params(format!("unknown kind `{kind}`: one of {}", kinds.join(", ")));
        if let Some(Value::Object(data)) = err.data.as_mut() {
            data.insert("reason".into(), Value::String("create_unknown_kind".into()));
        }
        return Err(err);
    }
    let at = match params.get("translation") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) if items.len() == 3 => {
            let mut v = [0.0_f32; 3];
            for (slot, item) in v.iter_mut().zip(items) {
                *slot = item.as_f64().filter(|x| x.is_finite()).ok_or_else(|| {
                    RpcError::invalid_params("`translation` must be [x, y, z] finite numbers")
                })? as f32;
            }
            Some(Vec3::from_array(v))
        }
        Some(_) => return Err(RpcError::invalid_params("`translation` must be [x, y, z]")),
    };
    if world.get_resource::<AppTypeRegistry>().is_none() {
        return Err(RpcError::internal("no edit world (no type registry)"));
    }
    let entity = create_kind(world, kind, at).map_err(RpcError::internal)?;
    let guid = world.get::<EntityGuid>(entity).map(|g| g.0.to_string());
    let undo_depth = world.get_resource::<UndoStack>().map_or(0, UndoStack::undo_depth);
    Ok(json!({
        "entity": entity.to_bits(),
        "guid": guid,
        "kind": kind,
        "name": crate::outliner::display_name(world, entity),
        "undo_depth": undo_depth,
    }))
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, crate::viewport_agent::Handler)> {
    vec![(
        MethodSchema {
            name: "editor.create_entity",
            tool: "create_entity",
            description: "Create one entry of the Create menu, as the menu does: an actor (player_start, trigger_volume = a 2 m sensor box collider, point_light, spot_light aimed down, directional_light, camera, reflection_capture = a LocalCapture with an unused id and a 4 m box, audio_emitter, rect_light aimed down, tube_light, decal = a StaticDecal of the built-in scorch prefab, post_process_volume = a default PostVolume, spline = two points 4 m apart, nav_link = a default NavLinkAnchor, smart_object = a SmartObject with no definition; each carries its runtime component's default) or a primitive shape (cuboid, sphere, ...), named after the entry, at the viewport's focus point (or `translation`), selected, one undo step. An unknown kind is refused with data.reason create_unknown_kind.",
            params: &[
                ParamSchema {
                    name: "kind",
                    kind: "string",
                    required: true,
                    description: "player_start, trigger_volume, point_light, spot_light, directional_light, camera, reflection_capture, audio_emitter, rect_light, tube_light, decal, post_process_volume, spline, nav_link, smart_object, or a shape kind (cuboid, sphere, capsule, cylinder, cone, conical_frustum, plane, torus, tetrahedron, ramp, prism).",
                },
                ParamSchema {
                    name: "translation",
                    kind: "array",
                    required: false,
                    description: "[x, y, z]; default the viewport's focus point (the orbit target).",
                },
            ],
            result: "{ entity, guid, kind, name, undo_depth }",
            mutating: true,
            permission: Permission::EditScene,
        },
        editor_create_entity as crate::viewport_agent::Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::App;

    /// w10-editor-agent-smalls (4): the schema text shows in the manual and the guide, so it carries no
    /// runs of whitespace (the description once held 17-space runs from a line continuation).
    #[test]
    fn schema_descriptions_have_no_whitespace_runs() {
        for (schema, _) in methods() {
            assert!(
                !schema.description.contains("  ") && !schema.description.contains('\n'),
                "{}: {:?}",
                schema.name,
                schema.description
            );
            for param in schema.params {
                assert!(
                    !param.description.contains("  "),
                    "{}.{}: {:?}",
                    schema.name,
                    param.name,
                    param.description
                );
            }
        }
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app
    }

    #[test]
    fn labels_are_sentence_case() {
        assert_eq!(shape_label("conical_frustum"), "Conical frustum");
        assert_eq!(shape_label("cuboid"), "Cuboid");
    }

    #[test]
    fn every_shape_creates_selects_and_undoes() {
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(EditorCamera {
            target: Vec3::new(3.0, 0.0, -2.0),
            ..EditorCamera::default()
        });
        for kind in PrimitiveShape::KINDS {
            let entity = create_shape(world, kind).expect("created");
            let primitive = world.get::<Primitive>(entity).expect("primitive spawned");
            assert_eq!(primitive.shape.kind(), *kind);
            assert_eq!(
                world.get::<Transform>(entity).map(|t| t.translation),
                Some(Vec3::new(3.0, 0.0, -2.0))
            );
            assert_eq!(world.resource::<Selection>().primary(), Some(entity));
        }
        let count = |world: &mut World| world.query::<&Primitive>().iter(world).count();
        assert_eq!(count(world), PrimitiveShape::KINDS.len());
        let mut undo = world.remove_resource::<UndoStack>().expect("undo stack");
        assert_eq!(undo.next_undo().as_deref(), Some("Create Prism"));
        undo.undo(world);
        assert_eq!(count(world), PrimitiveShape::KINDS.len() - 1);
        undo.redo(world);
        assert_eq!(count(world), PrimitiveShape::KINDS.len());
    }

    /// The component type paths and name a create left on `entity` (its scene record).
    fn record_of(world: &mut World, entity: Entity) -> (Option<String>, Vec<String>) {
        let file = crate::command::subtree_snapshot(world, &[entity.to_bits()]);
        let record = file.entities.first().expect("record");
        (record.name.clone(), record.components.keys().cloned().collect())
    }

    /// Every Create-menu entry (the menu calls [`create_actor`] / [`create_shape`]) and
    /// `editor.create_entity { kind }` make the same entity: same name, same components, at the
    /// focus point, selected, one undo step each (ADR-7468).
    #[test]
    fn create_entity_matches_every_menu_entry() {
        let mut app = app();
        let world = app.world_mut();
        let target = Vec3::new(-1.0, 2.0, 3.0);
        world.insert_resource(EditorCamera {
            target,
            ..EditorCamera::default()
        });
        let (_, handler) = methods().into_iter().next().expect("one method");
        for kind in create_kinds() {
            let menu = create_kind(world, kind, None).expect("menu entry");
            let depth = world.resource::<UndoStack>().undo_depth();
            let reply = handler(world, &serde_json::json!({ "kind": kind })).expect("agent");
            assert_eq!(reply["undo_depth"], depth + 1, "{kind}: one undo step");
            let agent =
                crate::command::entity_from_bits(reply["entity"].as_u64().expect("id")).expect("entity");
            assert_eq!(world.resource::<Selection>().primary(), Some(agent), "{kind}");
            assert_eq!(
                world.get::<Transform>(agent).map(|t| t.translation),
                Some(target),
                "{kind}: at the focus point"
            );
            assert_eq!(record_of(world, menu), record_of(world, agent), "{kind}");
        }
        assert_eq!(
            create_kinds().len(),
            ACTOR_ENTRIES.len() + PrimitiveShape::KINDS.len()
        );
        // The labels the menu shows are the entities' names.
        for action in ACTOR_ENTRIES {
            let entity = create_actor(world, actor_kind(action), None).expect("created");
            assert_eq!(record_of(world, entity).0.as_deref(), Some(action.label));
            assert_eq!(
                action.path,
                crate::agent_parity::ActionPath::Method("editor.create_entity")
            );
        }
        // A new spot light points at the floor; a trigger volume is a 2 m sensor box.
        let spot = create_actor(world, "spot_light", None).expect("spot");
        let down = world.get::<Transform>(spot).expect("t").rotation * Vec3::NEG_Z;
        assert!(down.abs_diff_eq(Vec3::NEG_Y, 1e-5), "{down}");
        let trigger = create_actor(world, "trigger_volume", None).expect("trigger");
        let collider = world.get::<regia_physics::Collider>(trigger).expect("collider");
        assert!(collider.sensor);
        assert_eq!(
            collider.shape,
            regia_physics::ColliderShape::Box {
                half_extents: Vec3::splat(TRIGGER_HALF_EXTENT)
            }
        );
        // A reflection capture is a Create entry now (it was refused while the editor could not
        // make one); an unknown kind is still refused.
        let capture = create_actor(world, "reflection_capture", None).expect("reflection capture");
        assert!(world.get::<regia_render::LocalCapture>(capture).is_some());
        assert!(create_actor(world, "reflection_probe", None).is_err());
    }

    /// Create ▸ Reflection capture: a `LocalCapture` with an id no other capture uses, its box at
    /// the focus point, selected, one undo step that removes it again.
    #[test]
    fn reflection_capture_creates_a_unique_capture_in_one_undo_step() {
        let mut app = app();
        let world = app.world_mut();
        let target = Vec3::new(2.0, 1.5, -4.0);
        world.insert_resource(EditorCamera {
            target,
            ..EditorCamera::default()
        });
        let first = create_kind(world, "reflection_capture", None).expect("first");
        let second = create_kind(world, "reflection_capture", None).expect("second");
        let id = |world: &World, e| world.get::<regia_render::LocalCapture>(e).map(|c| c.id);
        let (a, b) = (
            id(world, first).expect("capture"),
            id(world, second).expect("capture"),
        );
        assert_ne!(a, b, "every new capture gets its own id");
        assert!(a >= 1 && b >= 1, "ids start at 1 (0 is the component default)");
        let capture = *world.get::<regia_render::LocalCapture>(second).expect("capture");
        assert_eq!(capture.half_extent, [CAPTURE_HALF_EXTENT; 3]);
        assert!(capture.desc(target.to_array()).validate().is_ok());
        assert_eq!(
            world.get::<Transform>(second).map(|t| t.translation),
            Some(target)
        );
        assert_eq!(world.resource::<Selection>().primary(), Some(second));
        let count = |world: &mut World| world.query::<&regia_render::LocalCapture>().iter(world).count();
        assert_eq!(count(world), 2);
        let mut undo = world.remove_resource::<UndoStack>().expect("undo stack");
        assert_eq!(undo.next_undo().as_deref(), Some("Create Reflection capture"));
        undo.undo(world);
        assert_eq!(count(world), 1, "one undo step removes the capture");
        undo.redo(world);
        assert_eq!(count(world), 2);
    }

    /// ADR-7592 acceptance: the level a Create ▸ Reflection capture saves is byte for byte the
    /// level saved after the same component values are written through `editor.set_property`
    /// (the inspector's edit path): the create stores nothing the property path would not.
    #[test]
    fn reflection_capture_saves_the_bytes_set_property_writes() {
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(EditorCamera {
            target: Vec3::new(0.5, 1.0, -1.5),
            ..EditorCamera::default()
        });
        let open = crate::level_io::new_level(world);
        world.insert_resource(open.clone());
        let entity = create_kind(world, "reflection_capture", None).expect("capture");
        let created = *world.get::<regia_render::LocalCapture>(entity).expect("capture");
        let dir = regia_test_utils::unique_temp_dir("capture_level_bytes");
        let save = |world: &mut World, name: &str| {
            let path = dir.join(name);
            crate::level_io::save_level_file(world, &open, &path).expect("save");
            std::fs::read(&path).expect("read")
        };
        let menu_bytes = save(world, "menu.level.ron");
        let (_, set_property) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == "editor.set_property")
            .expect("editor.set_property");
        let set = |world: &mut World, path: &str, value: Value| {
            set_property(
                world,
                &json!({ "entity": entity.to_bits(), "component": "LocalCapture", "path": path, "value": value }),
            )
            .unwrap_or_else(|e| panic!("set {path}: {e:?}"));
        };
        // Other values first, so the second write is a real edit of every field.
        set(world, "id", json!(created.id + 40));
        set(world, "half_extent", json!([5.0, 6.0, 7.0]));
        set(world, "weight", json!(0.25));
        set(world, "fade", json!(3.0));
        assert_ne!(save(world, "changed.level.ron"), menu_bytes);
        set(world, "id", json!(created.id));
        set(world, "half_extent", json!(created.half_extent));
        set(world, "weight", json!(created.weight));
        set(world, "fade", json!(created.fade));
        assert_eq!(world.get::<regia_render::LocalCapture>(entity), Some(&created));
        let property_bytes = save(world, "property.level.ron");
        assert!(
            menu_bytes == property_bytes,
            "the created capture saves as the property path writes it"
        );
        let text = String::from_utf8_lossy(&menu_bytes);
        assert!(text.contains("LocalCapture"), "the capture is level content");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// One row per new Create entry: its kind, what the owning runtime plugin does for the
    /// component (`register_type`) and how many entities of a world carry it.
    type BreadthRow = (&'static str, fn(&mut App), fn(&mut World) -> usize);

    fn breadth_table() -> Vec<BreadthRow> {
        fn count<C: Component>(world: &mut World) -> usize {
            world.query::<&C>().iter(world).count()
        }
        macro_rules! row {
            ($kind:literal, $ty:ty) => {
                (
                    $kind,
                    (|app: &mut App| {
                        app.register_type::<$ty>();
                    }) as fn(&mut App),
                    count::<$ty> as fn(&mut World) -> usize,
                )
            };
        }
        vec![
            row!("audio_emitter", regia_audio::AudioEmitter),
            row!("rect_light", regia_render::RectLight),
            row!("tube_light", regia_render::TubeLight),
            row!("decal", regia_vfx::StaticDecal),
            row!("post_process_volume", regia_render::PostVolume),
            row!("spline", regia_scene::spline::Spline),
            row!("nav_link", regia_nav::NavLinkAnchor),
            row!("smart_object", regia_ai::SmartObject),
        ]
    }

    /// Saves the open level of a fresh edit world after `create` made one entity in it; returns the
    /// level text with the entity's guid replaced by a fixed word, and the entity.
    fn saved_level_of(kind: &str, create: impl FnOnce(&mut World) -> Entity) -> String {
        let mut app = app();
        let world = app.world_mut();
        world.insert_resource(EditorCamera {
            target: Vec3::new(1.5, 0.0, -2.5),
            ..EditorCamera::default()
        });
        let open = crate::level_io::new_level(world);
        world.insert_resource(open.clone());
        let entity = create(world);
        let guid = world.get::<EntityGuid>(entity).expect("guid").0.to_string();
        let dir = regia_test_utils::unique_temp_dir("create_breadth_level");
        let path = dir.join(format!("{kind}.level.ron"));
        crate::level_io::save_level_file(world, &open, &path).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::remove_dir_all(&dir).ok();
        assert!(text.contains(&guid), "{kind}: the saved level names the entity");
        text.replace(&guid, "ENTITY")
    }

    /// w11-create-menu-breadth: every new Create entry saves the same level bytes through the menu
    /// op ([`create_actor`], what the menu button calls) and through `editor.create_entity`, and the
    /// saved level loads in a world that registers the component as its runtime plugin does, with
    /// the component present on exactly the one entity, at the drop point; an unknown kind keeps
    /// its documented error.
    #[test]
    fn new_create_entries_save_the_same_bytes_by_menu_and_agent_and_load() {
        let (_, handler) = methods().into_iter().next().expect("one method");
        for (kind, register, count) in breadth_table() {
            let menu = saved_level_of(kind, |world| create_actor(world, kind, None).expect("menu"));
            let agent = saved_level_of(kind, |world| {
                let reply = handler(world, &json!({ "kind": kind })).expect("agent");
                crate::command::entity_from_bits(reply["entity"].as_u64().expect("id")).expect("entity")
            });
            assert_eq!(menu, agent, "{kind}: the same saved bytes");
            // Load the saved level into a fresh world.
            let dir = regia_test_utils::unique_temp_dir("create_breadth_load");
            let path = dir.join(format!("{kind}.level.ron"));
            let guid = "00000000-0000-0000-0000-00000000c0de";
            let text = menu.replace("ENTITY", guid);
            std::fs::write(&path, text).expect("write");
            let mut runtime = app();
            register(&mut runtime);
            let world = runtime.world_mut();
            crate::level_io::open_level_file(world, &path).unwrap_or_else(|e| panic!("{kind}: {e}"));
            assert_eq!(
                count(world),
                1,
                "{kind}: the runtime loads the component:\n{menu}"
            );
            let at = world
                .query::<(&Transform, &EntityGuid)>()
                .iter(world)
                .find(|(_, g)| g.0.to_string() == guid)
                .map(|(t, _)| t.translation);
            assert_eq!(at, Some(Vec3::new(1.5, 0.0, -2.5)), "{kind}: at the drop point");
            std::fs::remove_dir_all(&dir).ok();
        }
        // An unknown kind (including the two the editor cannot make yet) keeps its error.
        let mut app = app();
        for kind in ["water_plane", "fog_volume", "decals"] {
            let err = handler(app.world_mut(), &json!({ "kind": kind })).expect_err(kind);
            let data = err.data.as_ref().expect("data");
            assert_eq!(data["reason"], "create_unknown_kind", "{kind}");
            assert!(err.message.contains("one of"), "{kind}: {}", err.message);
        }
        for (kind, ..) in breadth_table() {
            assert!(create_kinds().contains(&kind), "{kind}");
        }
    }

    /// The seven entries that exist since ADR-7468/7592 and the eight of ADR-7741 each have one
    /// registry action `create.<kind>` that routes to `editor.create_entity`.
    #[test]
    fn every_actor_entry_is_a_registered_create_action() {
        assert_eq!(ACTOR_ENTRIES.len(), 15);
        for action in ACTOR_ENTRIES {
            assert_eq!(action.panel, "shell.create", "{}", action.id);
            assert!(create_kinds().contains(&actor_kind(action)));
        }
        let drawn: Vec<&str> = crate::agent_parity::ACTIONS
            .iter()
            .filter(|a| a.panel == "shell.create" && a.id != crate::agent_parity::CREATE_SHAPE.id)
            .map(|a| a.id)
            .collect();
        let entries: Vec<&str> = ACTOR_ENTRIES.iter().map(|a| a.id).collect();
        assert_eq!(drawn, entries, "the menu draws the registry's order");
    }

    #[test]
    fn unknown_shape_is_an_error() {
        let mut app = app();
        assert!(create_shape(app.world_mut(), "blob").is_err());
    }

    /// Calls the installed agent method `name` (the dispatcher's handler).
    fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = crate::agent_ext::all_methods()
            .into_iter()
            .find(|(schema, _)| schema.name == name)
            .unwrap_or_else(|| panic!("{name} is installed"));
        handler(world, &params)
    }

    /// Every capture id of the world, sorted.
    fn capture_ids(world: &mut World) -> Vec<u64> {
        let mut ids: Vec<u64> = world
            .query::<&regia_render::LocalCapture>()
            .iter(world)
            .map(|c| c.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn select_only(world: &mut World, entity: Entity) {
        let mut selection = Selection::default();
        selection.select(entity, SelectMode::Replace);
        world.insert_resource(selection);
    }

    /// ADR-7663 / w9 known issue: every copy point gives a copied reflection capture an id no
    /// other capture uses (Edit ▸ Duplicate, Copy then Paste, the gizmo's Alt-drag duplicate,
    /// the Outliner's Duplicate through `editor.duplicate` and through its command), each driven
    /// through the agent method a user's action takes, and the level still bakes afterwards.
    #[test]
    fn copied_reflection_captures_get_fresh_ids_on_every_copy_path() {
        let mut app = app();
        let world = app.world_mut();
        let original = create_kind(world, "reflection_capture", None).expect("capture");
        let original_id = world
            .get::<regia_render::LocalCapture>(original)
            .expect("capture")
            .id;
        let unique = |world: &mut World, path: &str| {
            let ids = capture_ids(world);
            let mut dedup = ids.clone();
            dedup.dedup();
            assert_eq!(ids, dedup, "{path}: every capture has its own id");
            ids.len()
        };
        let mut count = 1;
        let paths: [(&str, &str, Value); 5] = [
            (
                "Edit ▸ Duplicate",
                "editor.menu_invoke",
                json!({ "path": "Edit/Duplicate" }),
            ),
            (
                "gizmo Alt-drag",
                "editor.gizmo_drag",
                json!({ "entities": [original.to_bits()], "translate": [2.0, 0.0, 0.0], "duplicate": true }),
            ),
            (
                "Outliner Duplicate (Ctrl+D)",
                "editor.duplicate",
                json!({ "entity": original.to_bits() }),
            ),
            (
                "Outliner Duplicate (context menu command)",
                "editor.command",
                json!({ "run": "outliner.duplicate" }),
            ),
            (
                "Edit ▸ Copy",
                "editor.menu_invoke",
                json!({ "path": "Edit/Copy" }),
            ),
        ];
        for (path, method, params) in paths {
            select_only(world, original);
            call(world, method, params).unwrap_or_else(|e| panic!("{path}: {e:?}"));
            if method == "editor.menu_invoke" && path.ends_with("Copy") {
                call(world, "editor.menu_invoke", json!({ "path": "Edit/Paste" })).expect("paste");
                // Pasting twice makes two independent copies.
                call(world, "editor.menu_invoke", json!({ "path": "Edit/Paste" })).expect("paste");
                count += 1;
            }
            count += 1;
            assert_eq!(unique(world, path), count, "{path}: one more capture");
            assert_eq!(
                world.get::<regia_render::LocalCapture>(original).map(|c| c.id),
                Some(original_id),
                "{path}: the original keeps its id"
            );
        }
        // Undo removes the last paste's copy, redo brings it back with the same new id.
        let before = capture_ids(world);
        assert!(crate::command::undo_world(world));
        assert_eq!(capture_ids(world).len(), before.len() - 1);
        assert!(crate::command::redo_world(world));
        assert_eq!(capture_ids(world), before, "redo restores the copy's new id");
        // The bake no longer refuses the level.
        let root = regia_test_utils::unique_temp_dir("capture_copy_ids");
        std::fs::create_dir_all(root.join("assets")).expect("assets");
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(root.clone()),
        });
        world.init_resource::<regia_render::RuntimeRenderer>();
        let started = crate::capture_bake::start(world, Some("maps/a.fcap")).map_err(|e| e.reason);
        assert_eq!(started, Ok(()));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A Cut then Paste moves a capture: its id is free again, so the pasted capture keeps it
    /// (and its bake still matches); a second Paste of the same clipboard takes a new one.
    #[test]
    fn cut_then_paste_keeps_the_capture_id() {
        let mut app = app();
        let world = app.world_mut();
        let original = create_kind(world, "reflection_capture", None).expect("capture");
        let id = world
            .get::<regia_render::LocalCapture>(original)
            .expect("capture")
            .id;
        select_only(world, original);
        call(world, "editor.menu_invoke", json!({ "path": "Edit/Cut" })).expect("cut");
        assert!(capture_ids(world).is_empty());
        call(world, "editor.menu_invoke", json!({ "path": "Edit/Paste" })).expect("paste");
        assert_eq!(capture_ids(world), [id]);
        call(world, "editor.menu_invoke", json!({ "path": "Edit/Paste" })).expect("paste");
        assert_eq!(capture_ids(world), [id, id + 1]);
    }

    /// The helper reaches captures inside nested records and leaves other components alone.
    #[test]
    fn fresh_capture_ids_renumbers_in_file_order_and_skips_other_records() {
        let mut app = app();
        let world = app.world_mut();
        let a = create_kind(world, "reflection_capture", None).expect("capture");
        let b = create_kind(world, "reflection_capture", None).expect("capture");
        let light = create_kind(world, "point_light", None).expect("light");
        let file = crate::command::subtree_snapshot(world, &[a.to_bits(), b.to_bits()]);
        let mut records = file.entities.clone();
        let mut changed = fresh_capture_ids(world, &mut records);
        changed.sort_unstable();
        // Both ids are live, so both copies take the next free ids, one past the largest.
        let olds: Vec<u64> = changed.iter().map(|c| c.0).collect();
        let mut news: Vec<u64> = changed.iter().map(|c| c.1).collect();
        news.sort_unstable();
        assert_eq!((olds, news), (vec![1, 2], vec![3, 4]));
        // Nothing else of the records moved.
        for (old, new) in file.entities.iter().zip(&records) {
            assert_eq!(old.guid, new.guid);
            assert_eq!(
                old.components.keys().collect::<Vec<_>>(),
                new.components.keys().collect::<Vec<_>>()
            );
        }
        let mut lights = crate::command::subtree_snapshot(world, &[light.to_bits()]).entities;
        let before = lights.clone();
        assert!(fresh_capture_ids(world, &mut lights).is_empty());
        assert_eq!(
            ron::to_string(&before).ok(),
            ron::to_string(&lights).ok(),
            "a record without a capture is untouched"
        );
    }
}
