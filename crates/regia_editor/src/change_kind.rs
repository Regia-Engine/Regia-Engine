// SPDX-License-Identifier: MPL-2.0
//! Change an entity's kind in place (`docs/specs/19-editor.md` § 2, ADR-7772): swap a light for
//! another light type, a collider's shape for another shape, or a camera's projection, keeping the
//! fields the two share (same name and same type) and the entity's children, as one undo step.
//!
//! Light swaps go through reflection: the new light starts at its type's default and takes every
//! field of the old one with the same name and type. One rename is applied on top: a point light's
//! `radius` and the other lights' `range` are the same quantity (a cutoff distance in metres), so
//! a point light turned into a spot light keeps its range. Collider shapes keep `radius`,
//! `half_height` and `axis` where both shapes have them; the rest of the collider (offset,
//! material, layer, events) is untouched.

use regia_ecs::ecs::component::ComponentId;
use regia_ecs::prelude::*;
use regia_ecs::reflect::std_traits::ReflectDefault;
use regia_ecs::reflect::{AppTypeRegistry, PartialReflect, ReflectComponent, ReflectRef};

use crate::command::{
    AddComponent, CommandGroup, EditorCommand as _, RemoveComponent, SetProperty, UndoStack,
};

/// The light kinds: kind name, component type name, menu label.
pub const LIGHT_KINDS: [(&str, &str, &str); 5] = [
    ("point_light", "PointLight", "Point light"),
    ("spot_light", "SpotLight", "Spot light"),
    ("rect_light", "RectLight", "Rectangle light"),
    ("tube_light", "TubeLight", "Tube light"),
    ("directional_light", "DirectionalLight", "Directional light"),
];

/// The collider shape kinds: kind name, menu label.
pub const COLLIDER_KINDS: [(&str, &str); 5] = [
    ("collider_box", "Box collider"),
    ("collider_sphere", "Sphere collider"),
    ("collider_capsule", "Capsule collider"),
    ("collider_cylinder", "Cylinder collider"),
    ("collider_cone", "Cone collider"),
];

/// The camera projection kinds: kind name, menu label.
pub const CAMERA_KINDS: [(&str, &str); 2] = [
    ("camera_perspective", "Perspective camera"),
    ("camera_orthographic", "Orthographic camera"),
];

/// Field renames applied on top of the same-name rule: (source field, target field).
const LIGHT_ALIASES: [(&str, &str); 2] = [("radius", "range"), ("range", "radius")];

/// Why a kind change was refused (each has a stable lowercase code).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChangeKindError {
    /// The entity does not exist.
    #[error("entity {0} does not exist")]
    NoSuchEntity(u64),
    /// The kind name is not one of the known kinds.
    #[error("unknown kind `{0}`")]
    UnknownKind(String),
    /// The entity has nothing the kind can replace (no light for a light kind, …).
    #[error("entity {0} has nothing `{1}` can replace")]
    NotCompatible(u64, String),
    /// The entity already is that kind.
    #[error("entity {0} already is `{1}`")]
    AlreadyThatKind(u64, String),
    /// Reflection could not read or build a value (an unregistered type).
    #[error("reflection: {0}")]
    Reflect(String),
}

impl ChangeKindError {
    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoSuchEntity(_) => "no_such_entity",
            Self::UnknownKind(_) => "unknown_kind",
            Self::NotCompatible(..) => "kind_not_compatible",
            Self::AlreadyThatKind(..) => "already_that_kind",
            Self::Reflect(_) => "kind_reflect_failed",
        }
    }
}

/// What a kind change did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindChange {
    /// The kind before.
    pub from: String,
    /// The kind after.
    pub to: String,
    /// The fields carried over (target field names).
    pub kept: Vec<String>,
}

/// The entity's current kind among the changeable ones (light, collider shape, camera projection),
/// in that order of precedence.
#[must_use]
pub fn current_kinds(world: &World, entity: Entity) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(kind) = current_light(world, entity) {
        out.push(kind.0);
    }
    if let Some(collider) = world.get::<regia_physics::Collider>(entity)
        && let Some(kind) = shape_kind(&collider.shape)
    {
        out.push(kind);
    }
    if let Some(camera) = world.get::<regia_render::Camera>(entity) {
        out.push(if camera.orthographic {
            "camera_orthographic"
        } else {
            "camera_perspective"
        });
    }
    out
}

/// The kinds `entity` can change to, with their menu labels (other lights for a light, other
/// shapes for a collider, the other projection for a camera).
#[must_use]
pub fn compatible_kinds(world: &World, entity: Entity) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if let Some((current, _, _)) = current_light(world, entity) {
        out.extend(
            LIGHT_KINDS
                .iter()
                .filter(|(kind, _, _)| *kind != current)
                .map(|(kind, _, label)| (*kind, *label)),
        );
    }
    if let Some(collider) = world.get::<regia_physics::Collider>(entity) {
        let current = shape_kind(&collider.shape);
        out.extend(
            COLLIDER_KINDS
                .iter()
                .filter(|(kind, _)| Some(*kind) != current)
                .copied(),
        );
    }
    if let Some(camera) = world.get::<regia_render::Camera>(entity) {
        let current = if camera.orthographic {
            "camera_orthographic"
        } else {
            "camera_perspective"
        };
        out.extend(CAMERA_KINDS.iter().filter(|(kind, _)| *kind != current).copied());
    }
    out
}

fn current_light(world: &World, entity: Entity) -> Option<(&'static str, &'static str, &'static str)> {
    let has = |name: &str| match name {
        "PointLight" => world.get::<regia_render::PointLight>(entity).is_some(),
        "SpotLight" => world.get::<regia_render::SpotLight>(entity).is_some(),
        "RectLight" => world.get::<regia_render::RectLight>(entity).is_some(),
        "TubeLight" => world.get::<regia_render::TubeLight>(entity).is_some(),
        "DirectionalLight" => world.get::<regia_render::DirectionalLight>(entity).is_some(),
        _ => false,
    };
    LIGHT_KINDS
        .iter()
        .find(|(_, component, _)| has(component))
        .copied()
}

fn shape_kind(shape: &regia_physics::ColliderShape) -> Option<&'static str> {
    use regia_physics::ColliderShape as S;
    Some(match shape {
        S::Box { .. } => "collider_box",
        S::Sphere { .. } => "collider_sphere",
        S::Capsule { .. } => "collider_capsule",
        S::Cylinder { .. } => "collider_cylinder",
        S::Cone { .. } => "collider_cone",
        _ => return None,
    })
}

/// The shape of kind `kind`, keeping `radius`, `half_height` and `axis` from `old` where both
/// shapes have them (defaults: 0.5 m half extents and radius, 0.5 m half height, the Y axis).
fn convert_shape(
    old: &regia_physics::ColliderShape,
    kind: &str,
) -> Option<(regia_physics::ColliderShape, Vec<String>)> {
    use regia_physics::{ColliderShape as S, ShapeAxis};
    let (radius, half_height, axis) = match old {
        S::Sphere { radius } => (Some(*radius), None, None),
        S::Capsule {
            half_height,
            radius,
            axis,
        }
        | S::Cylinder {
            half_height,
            radius,
            axis,
        }
        | S::Cone {
            half_height,
            radius,
            axis,
        } => (Some(*radius), Some(*half_height), Some(*axis)),
        _ => (None, None, None),
    };
    let mut kept: Vec<String> = Vec::new();
    let r = radius.unwrap_or(0.5);
    let shape = match kind {
        "collider_box" => S::Box {
            half_extents: regia_core::math::Vec3::splat(0.5),
        },
        "collider_sphere" => {
            if radius.is_some() {
                kept.push("radius".into());
            }
            S::Sphere { radius: r }
        }
        "collider_capsule" | "collider_cylinder" | "collider_cone" => {
            for (name, present) in [
                ("radius", radius.is_some()),
                ("half_height", half_height.is_some()),
                ("axis", axis.is_some()),
            ] {
                if present {
                    kept.push(name.into());
                }
            }
            let (half_height, axis) = (half_height.unwrap_or(0.5), axis.unwrap_or(ShapeAxis::Y));
            match kind {
                "collider_capsule" => S::Capsule {
                    half_height,
                    radius: r,
                    axis,
                },
                "collider_cylinder" => S::Cylinder {
                    half_height,
                    radius: r,
                    axis,
                },
                _ => S::Cone {
                    half_height,
                    radius: r,
                    axis,
                },
            }
        }
        _ => return None,
    };
    Some((shape, kept))
}

/// The type path of a light component's short name (as [`LIGHT_KINDS`] lists it).
fn light_type_path(short: &str) -> Option<&'static str> {
    Some(match short {
        "PointLight" => std::any::type_name::<regia_render::PointLight>(),
        "SpotLight" => std::any::type_name::<regia_render::SpotLight>(),
        "RectLight" => std::any::type_name::<regia_render::RectLight>(),
        "TubeLight" => std::any::type_name::<regia_render::TubeLight>(),
        "DirectionalLight" => std::any::type_name::<regia_render::DirectionalLight>(),
        _ => return None,
    })
}

/// The registration of `name`: a full type path first, then a short one.
fn registration<'a>(
    registry: &'a regia_ecs::reflect::TypeRegistry,
    name: &str,
) -> Option<&'a regia_ecs::reflect::TypeRegistration> {
    registry
        .get_with_type_path(name)
        .or_else(|| registry.get_with_short_type_path(name))
}

/// The default value of the registered component `target`, with every field of `entity`'s `source`
/// component that has the same name (or an aliased one) and the same type copied in. Both names
/// are type paths (full, or short when unambiguous). Returns the value and the copied field names.
///
/// # Errors
/// When a type is not registered (with a default for `target`), or the entity lacks `source`.
pub fn carry_fields(
    world: &World,
    entity: Entity,
    source: &str,
    target: &str,
    aliases: &[(&str, &str)],
) -> Result<(Box<dyn PartialReflect>, Vec<String>), ChangeKindError> {
    let registry = world
        .get_resource::<AppTypeRegistry>()
        .ok_or_else(|| ChangeKindError::Reflect("no type registry".into()))?
        .clone();
    let registry = registry.read();
    let default = registration(&registry, target)
        .and_then(|r| r.data::<ReflectDefault>())
        .ok_or_else(|| ChangeKindError::Reflect(format!("{target} is not registered with a default")))?;
    let source_data = registration(&registry, source)
        .and_then(|r| r.data::<ReflectComponent>())
        .ok_or_else(|| ChangeKindError::Reflect(format!("{source} is not a registered component")))?;
    let entity_ref = world
        .get_entity(entity)
        .map_err(|_| ChangeKindError::NoSuchEntity(entity.to_bits()))?;
    let source_value = source_data
        .reflect(entity_ref)
        .ok_or_else(|| ChangeKindError::NotCompatible(entity.to_bits(), target.to_string()))?;
    let mut value = default.default().into_partial_reflect();
    let mut kept = Vec::new();
    let ReflectRef::Struct(from) = source_value.reflect_ref() else {
        return Ok((value, kept));
    };
    if let regia_ecs::reflect::ReflectMut::Struct(to) = value.reflect_mut() {
        for index in 0..to.field_len() {
            let Some(name) = to.name_at(index).map(str::to_string) else {
                continue;
            };
            let source_field = from.field(&name).or_else(|| {
                aliases
                    .iter()
                    .filter(|(_, alias_to)| *alias_to == name)
                    .find_map(|(alias_from, _)| from.field(alias_from))
            });
            let (Some(source_field), Some(target_field)) = (source_field, to.field_at_mut(index)) else {
                continue;
            };
            let same_type = match (
                source_field.get_represented_type_info(),
                target_field.get_represented_type_info(),
            ) {
                (Some(a), Some(b)) => a.type_id() == b.type_id(),
                _ => false,
            };
            if same_type && target_field.try_apply(source_field).is_ok() {
                kept.push(name);
            }
        }
    }
    Ok((value, kept))
}

/// The components `entity` has now (empty for a missing entity).
fn component_ids(world: &World, entity: Entity) -> Vec<ComponentId> {
    world
        .get_entity(entity)
        .map(|e| e.archetype().components().to_vec())
        .unwrap_or_default()
}

/// The undo half of a light swap's required components: applying does nothing (inserting the new
/// light inserts them again), reverting removes the components the new light brought in, so the
/// entity gets back exactly the component set it had.
struct DropRequiredOnUndo {
    entity: u64,
    required: Vec<ComponentId>,
}

impl crate::command::EditorCommand for DropRequiredOnUndo {
    fn apply(&mut self, _world: &mut World) {}

    fn revert(&mut self, world: &mut World) {
        let Some(entity) = crate::command::resolve_entity(world, self.entity) else {
            return;
        };
        if let Ok(mut entity) = world.get_entity_mut(entity) {
            for id in &self.required {
                entity.remove_by_id(*id);
            }
        }
    }

    fn merge(&mut self, _next: &dyn crate::command::EditorCommand) -> bool {
        false
    }

    fn description(&self) -> String {
        "Drop required components".into()
    }
}

/// Changes `entity` to `kind` (see [`compatible_kinds`]) as one undo step on the [`UndoStack`].
///
/// # Errors
/// [`ChangeKindError`]: an unknown entity or kind, a kind the entity cannot take, or the kind it
/// already is.
pub fn change_kind(world: &mut World, entity: Entity, kind: &str) -> Result<KindChange, ChangeKindError> {
    let bits = entity.to_bits();
    if world.get_entity(entity).is_err() {
        return Err(ChangeKindError::NoSuchEntity(bits));
    }
    let known = LIGHT_KINDS.iter().any(|(k, _, _)| *k == kind)
        || COLLIDER_KINDS.iter().any(|(k, _)| *k == kind)
        || CAMERA_KINDS.iter().any(|(k, _)| *k == kind);
    if !known {
        return Err(ChangeKindError::UnknownKind(kind.to_string()));
    }
    if current_kinds(world, entity).contains(&kind) {
        return Err(ChangeKindError::AlreadyThatKind(bits, kind.to_string()));
    }
    let reflect = |e: crate::command::CommandError| ChangeKindError::Reflect(e.to_string());
    let mut group = CommandGroup::new("Change kind");
    let change = if let Some((_, target, _)) = LIGHT_KINDS.iter().find(|(k, _, _)| *k == kind) {
        let Some((from_kind, from_component, _)) = current_light(world, entity) else {
            return Err(ChangeKindError::NotCompatible(bits, kind.to_string()));
        };
        let (Some(from_path), Some(target_path)) = (light_type_path(from_component), light_type_path(target))
        else {
            return Err(ChangeKindError::UnknownKind(kind.to_string()));
        };
        let (value, kept) = carry_fields(world, entity, from_path, target_path, &LIGHT_ALIASES)?;
        let mut remove = RemoveComponent::capture(world, entity, from_path).map_err(reflect)?;
        let mut add = AddComponent::capture(world, entity, target_path, value).map_err(reflect)?;
        let before = component_ids(world, entity);
        remove.apply(world);
        add.apply(world);
        group.push(Box::new(remove));
        group.push(Box::new(add));
        // The new light's required components (a directional light requires
        // `InheritedVisibility`) arrive with it and must leave with it on undo.
        let target_id = world
            .get_resource::<AppTypeRegistry>()
            .and_then(|r| {
                registration(&r.read(), target_path).map(regia_ecs::reflect::TypeRegistration::type_id)
            })
            .and_then(|type_id| world.components().get_id(type_id));
        let required: Vec<ComponentId> = component_ids(world, entity)
            .into_iter()
            .filter(|id| !before.contains(id) && Some(*id) != target_id)
            .collect();
        if !required.is_empty() {
            group.push(Box::new(DropRequiredOnUndo {
                entity: bits,
                required,
            }));
        }
        KindChange {
            from: from_kind.to_string(),
            to: kind.to_string(),
            kept,
        }
    } else if kind.starts_with("collider_") {
        let Some(collider) = world.get::<regia_physics::Collider>(entity).cloned() else {
            return Err(ChangeKindError::NotCompatible(bits, kind.to_string()));
        };
        let from = shape_kind(&collider.shape).unwrap_or("collider_other");
        let Some((shape, kept)) = convert_shape(&collider.shape, kind) else {
            return Err(ChangeKindError::UnknownKind(kind.to_string()));
        };
        let new = regia_physics::Collider { shape, ..collider };
        let mut set = SetProperty::capture(
            world,
            entity,
            std::any::type_name::<regia_physics::Collider>(),
            "",
            Box::new(new),
        )
        .map_err(reflect)?;
        set.coalesce = false;
        set.apply(world);
        group.push(Box::new(set));
        KindChange {
            from: from.to_string(),
            to: kind.to_string(),
            kept,
        }
    } else {
        if world.get::<regia_render::Camera>(entity).is_none() {
            return Err(ChangeKindError::NotCompatible(bits, kind.to_string()));
        }
        let orthographic = kind == "camera_orthographic";
        let mut set = SetProperty::capture(
            world,
            entity,
            std::any::type_name::<regia_render::Camera>(),
            "orthographic",
            Box::new(orthographic),
        )
        .map_err(reflect)?;
        set.coalesce = false;
        set.apply(world);
        group.push(Box::new(set));
        KindChange {
            from: if orthographic {
                "camera_perspective"
            } else {
                "camera_orthographic"
            }
            .to_string(),
            to: kind.to_string(),
            kept: vec!["fov_y".into(), "ortho_size".into(), "near".into(), "far".into()],
        }
    };
    world
        .get_resource_or_insert_with::<UndoStack>(UndoStack::new)
        .push(Box::new(group));
    Ok(change)
}

/// `editor.change_kind`.
fn change_kind_method(
    world: &mut World,
    params: &serde_json::Value,
) -> Result<serde_json::Value, regia_agent::protocol::RpcError> {
    use regia_agent::protocol::{INVALID_PARAMS, RpcError};
    use serde_json::json;
    let entity = params
        .get("entity")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| RpcError::wrong_type("entity", "an entity id"))?;
    let entity = crate::command::resolve_entity(world, entity).ok_or_else(|| {
        RpcError::new(
            INVALID_PARAMS,
            "no_such_entity",
            format!("no entity {entity}"),
            "list entities with editor.hierarchy",
        )
    })?;
    let Some(kind) = params.get("kind").and_then(serde_json::Value::as_str) else {
        // Without a kind: list what the entity can become.
        return Ok(json!({
            "entity": entity.to_bits(),
            "current": current_kinds(world, entity),
            "compatible": compatible_kinds(world, entity).iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        }));
    };
    match change_kind(world, entity, kind) {
        Ok(change) => Ok(json!({
            "entity": entity.to_bits(),
            "from": change.from,
            "to": change.to,
            "kept": change.kept,
            "current": current_kinds(world, entity),
            "compatible": compatible_kinds(world, entity).iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        })),
        Err(err) => Err(RpcError::new(
            INVALID_PARAMS,
            err.code(),
            err.to_string(),
            "call editor.change_kind without `kind` to list the compatible kinds",
        )),
    }
}

/// The change-kind surface (`editor.change_kind`).
#[must_use]
pub fn methods() -> Vec<(regia_agent::schema::MethodSchema, crate::viewport_agent::Handler)> {
    use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
    vec![(
        MethodSchema {
            name: "editor.change_kind",
            tool: "change_kind",
            description: "Change an entity's kind in place as one undo step, keeping the fields the two kinds share \
                (same name and type; a point light's radius is a spot light's range) and its children: a light to \
                another light type (point_light, spot_light, rect_light, tube_light, directional_light), a \
                collider to another shape (collider_box, collider_sphere, collider_capsule, collider_cylinder, \
                collider_cone), a camera to the other projection (camera_perspective, camera_orthographic). \
                Without `kind`, lists the entity's current and compatible kinds.",
            params: &[
                ParamSchema {
                    name: "entity",
                    kind: "integer",
                    required: true,
                    description: "Entity id.",
                },
                ParamSchema {
                    name: "kind",
                    kind: "string",
                    required: false,
                    description: "The new kind; absent = list the compatible kinds.",
                },
            ],
            result: "{ entity, from?, to?, kept?, current, compatible }",
            mutating: true,
            permission: Permission::EditScene,
        },
        change_kind_method as crate::viewport_agent::Handler,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::prelude::{EntityGuid, Name, Transform};

    fn app() -> regia_app::App {
        let mut app = regia_app::App::new();
        app.add_plugins(regia_scene::ScenePlugin);
        app.register_type::<regia_render::PointLight>()
            .register_type::<regia_render::SpotLight>()
            .register_type::<regia_render::RectLight>()
            .register_type::<regia_render::TubeLight>()
            .register_type::<regia_render::DirectionalLight>()
            .register_type::<regia_render::Camera>()
            .register_type::<regia_physics::Collider>();
        app
    }

    fn snapshot(world: &mut World, entity: Entity) -> String {
        crate::command::subtree_snapshot(world, &[entity.to_bits()])
            .to_ron_string()
            .unwrap_or_else(|e| format!("snapshot failed: {e}"))
    }

    #[test]
    fn point_light_to_spot_light_keeps_color_intensity_and_range_and_undo_restores_the_bytes() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = world
            .spawn((
                Name::new("Lamp"),
                EntityGuid::new_random(),
                Transform::default(),
                regia_render::PointLight {
                    position: [0.0; 3],
                    color: [0.9, 0.5, 0.2],
                    intensity: 1234.0,
                    radius: 7.5,
                    cast_shadows: true,
                },
            ))
            .id();
        let bulb = world
            .spawn((Name::new("Bulb"), EntityGuid::new_random(), Transform::default()))
            .id();
        world.entity_mut(bulb).insert(ChildOf(lamp));
        let before = snapshot(world, lamp);
        assert!(before.contains("PointLight"), "{before}");
        assert_eq!(
            compatible_kinds(world, lamp)
                .iter()
                .map(|(k, _)| *k)
                .collect::<Vec<_>>(),
            ["spot_light", "rect_light", "tube_light", "directional_light"]
        );

        let change = change_kind(world, lamp, "spot_light").expect("point to spot");
        assert_eq!(change.from, "point_light");
        for field in ["color", "intensity", "range", "cast_shadows"] {
            assert!(
                change.kept.iter().any(|k| k == field),
                "{field} kept: {:?}",
                change.kept
            );
        }
        assert!(world.get::<regia_render::PointLight>(lamp).is_none());
        let spot = world
            .get::<regia_render::SpotLight>(lamp)
            .cloned()
            .expect("a spot light");
        assert_eq!(spot.color, [0.9, 0.5, 0.2]);
        assert_eq!(spot.intensity, 1234.0);
        assert_eq!(spot.range, 7.5);
        assert!(spot.cast_shadows);
        // Children stay.
        assert_eq!(world.get::<ChildOf>(bulb).map(ChildOf::parent), Some(lamp));
        // One undo step restores the entity byte for byte.
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(crate::command::undo_world(world));
        assert_eq!(snapshot(world, lamp), before);
        assert!(world.get::<regia_render::SpotLight>(lamp).is_none());
        // Redo brings the spot back.
        assert!(crate::command::redo_world(world));
        assert_eq!(
            world.get::<regia_render::SpotLight>(lamp).map(|s| s.range),
            Some(7.5)
        );
    }

    /// Review finding 3 (rev-g3): a directional light requires `InheritedVisibility`; undoing
    /// point → directional must not leave it on the point light.
    #[test]
    fn point_light_to_directional_light_and_undo_restores_the_component_set() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = world
            .spawn((
                Name::new("Lamp"),
                EntityGuid::new_random(),
                Transform::default(),
                regia_render::PointLight {
                    position: [0.0; 3],
                    color: [0.9, 0.5, 0.2],
                    intensity: 1234.0,
                    radius: 7.5,
                    cast_shadows: true,
                },
            ))
            .id();
        // Since w10-lights-visibility (03f6f5c19, ADR-7670) every light kind requires
        // `InheritedVisibility`, so the point light has it too and the swap brings no new
        // component; the claim stays that undo gives back exactly the component set and values
        // (the drop-on-undo half for a target with a requirement the source lacks is tested
        // directly in `drop_required_on_undo_removes_only_what_the_new_light_brought`).
        assert!(
            world
                .get::<regia_scene::components::InheritedVisibility>(lamp)
                .is_some(),
            "a point light requires InheritedVisibility (03f6f5c19)"
        );
        let mut components_before = world.entity(lamp).archetype().components().to_vec();
        components_before.sort();
        let before = snapshot(world, lamp);
        let change = change_kind(world, lamp, "directional_light").expect("point to directional");
        assert!(change.kept.iter().any(|k| k == "color"), "{:?}", change.kept);
        assert!(world.get::<regia_render::DirectionalLight>(lamp).is_some());
        assert!(world.get::<regia_render::PointLight>(lamp).is_none());
        assert!(
            world
                .get::<regia_scene::components::InheritedVisibility>(lamp)
                .is_some(),
            "the directional light's required component is there"
        );
        assert_eq!(world.resource::<UndoStack>().undo_depth(), 1);
        assert!(crate::command::undo_world(world));
        assert!(world.get::<regia_render::DirectionalLight>(lamp).is_none());
        let mut components_after = world.entity(lamp).archetype().components().to_vec();
        components_after.sort();
        assert_eq!(components_after, components_before);
        assert_eq!(snapshot(world, lamp), before);
        // Redo brings the directional light back; undo again restores the same set.
        assert!(crate::command::redo_world(world));
        assert!(world.get::<regia_render::DirectionalLight>(lamp).is_some());
        assert!(crate::command::undo_world(world));
        let mut components_again = world.entity(lamp).archetype().components().to_vec();
        components_again.sort();
        assert_eq!(components_again, components_before);
        assert_eq!(
            world.get::<regia_render::PointLight>(lamp).map(|p| p.radius),
            Some(7.5)
        );
    }

    /// Review finding 3 (rev-g3), the mechanism: a light swap whose target brought components
    /// the entity did not have records them, and its undo removes exactly those (applying is a
    /// no-op, since inserting the target inserts them again).
    #[test]
    fn drop_required_on_undo_removes_only_what_the_new_light_brought() {
        let mut app = app();
        let world = app.world_mut();
        let lamp = world
            .spawn((
                Name::new("Lamp"),
                regia_scene::components::InheritedVisibility::default(),
            ))
            .id();
        let visibility = world
            .components()
            .get_id(std::any::TypeId::of::<regia_scene::components::InheritedVisibility>())
            .expect("registered");
        let mut step = DropRequiredOnUndo {
            entity: lamp.to_bits(),
            required: vec![visibility],
        };
        crate::command::EditorCommand::apply(&mut step, world);
        assert!(
            world
                .get::<regia_scene::components::InheritedVisibility>(lamp)
                .is_some(),
            "applying leaves the components alone"
        );
        crate::command::EditorCommand::revert(&mut step, world);
        assert!(
            world
                .get::<regia_scene::components::InheritedVisibility>(lamp)
                .is_none(),
            "undo drops the brought-in component"
        );
        assert!(world.get::<Name>(lamp).is_some(), "and nothing else");
    }

    #[test]
    fn collider_shapes_and_camera_projection_change_in_place() {
        let mut app = app();
        let world = app.world_mut();
        let ball = world
            .spawn((
                Name::new("Ball"),
                EntityGuid::new_random(),
                regia_physics::Collider {
                    shape: regia_physics::ColliderShape::Sphere { radius: 0.3 },
                    friction: 0.9,
                    ..Default::default()
                },
            ))
            .id();
        let change = change_kind(world, ball, "collider_capsule").expect("sphere to capsule");
        assert_eq!(change.kept, ["radius"]);
        let collider = world
            .get::<regia_physics::Collider>(ball)
            .cloned()
            .expect("collider");
        assert!(matches!(
            collider.shape,
            regia_physics::ColliderShape::Capsule { radius, .. } if radius == 0.3
        ));
        assert_eq!(collider.friction, 0.9);
        let camera = world
            .spawn((
                Name::new("Cam"),
                EntityGuid::new_random(),
                regia_render::Camera::default(),
            ))
            .id();
        change_kind(world, camera, "camera_orthographic").expect("ortho");
        assert!(
            world
                .get::<regia_render::Camera>(camera)
                .is_some_and(|c| c.orthographic)
        );
        assert_eq!(
            change_kind(world, camera, "camera_orthographic").map_err(|e| e.code()),
            Err("already_that_kind")
        );
        assert_eq!(
            change_kind(world, camera, "spot_light").map_err(|e| e.code()),
            Err("kind_not_compatible")
        );
        assert_eq!(
            change_kind(world, camera, "banana").map_err(|e| e.code()),
            Err("unknown_kind")
        );
        crate::command::undo_world(world);
        assert!(
            world
                .get::<regia_render::Camera>(camera)
                .is_some_and(|c| !c.orthographic)
        );
    }
}
