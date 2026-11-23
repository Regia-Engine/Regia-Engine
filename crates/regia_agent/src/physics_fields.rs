// SPDX-License-Identifier: MPL-2.0
//! `physics.fields`, `physics.field_set` and `physics.field_probe` (ADR-3348,
//! `book/agents/physics-fields.md`): list, create/edit/remove and probe the gameplay force
//! fields of `regia_physics::fields`.
//!
//! Malformed JSON is a `F0002` error and a missing entity `F0003`; a value the field cannot
//! take comes back as `{ status: "failed", reason }` with a lowercase code (`unknown_kind`,
//! `unknown_volume`, `unknown_falloff`, `unknown_mode`, `missing_kind`, `non_finite_input`,
//! `invalid_value`, `no_field`).

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::fields::{
    BodySample, Falloff, FieldKind, FieldMode, FieldPose, FieldVolume, ForceField, ancestor_world_affine,
    field_pose, localize_under_parent, probe_fields, world_field_pose,
};
use regia_scene::{GlobalTransform, Transform};
use serde_json::{Map, Value, json};

use crate::params::{opt_bool, opt_i64, opt_str, opt_u64};
use crate::protocol::RpcError;
use regia_ecs::prelude::ChildOf;

/// Most `(distance, factor)` points a curve falloff may have.
pub(crate) const MAX_CURVE_POINTS: usize = 64;

/// A failed-but-well-formed answer: `{ status: "failed", reason }`.
fn failed(reason: &str, detail: impl Into<String>) -> Value {
    json!({ "status": "failed", "reason": reason, "detail": detail.into() })
}

/// A parse problem: either malformed JSON (an `RpcError`) or a refusal with a reason code.
enum Bad {
    Rpc(RpcError),
    Refused(&'static str, String),
}

impl From<RpcError> for Bad {
    fn from(e: RpcError) -> Self {
        Bad::Rpc(e)
    }
}

type Parsed<T> = Result<T, Bad>;

fn finite(n: f64, what: &str) -> Parsed<f32> {
    let n = n as f32;
    if n.is_finite() {
        Ok(n)
    } else {
        Err(Bad::Refused(
            "non_finite_input",
            format!("`{what}` is not finite"),
        ))
    }
}

fn number(value: &Value, what: &str) -> Parsed<f32> {
    let n = value
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be a number")))?;
    finite(n, what)
}

fn vec3(value: &Value, what: &str) -> Parsed<Vec3> {
    let Some(items) = value.as_array().filter(|a| a.len() == 3) else {
        return Err(RpcError::invalid_params(format!("`{what}` must be [x, y, z]")).into());
    };
    Ok(Vec3::new(
        number(&items[0], what)?,
        number(&items[1], what)?,
        number(&items[2], what)?,
    ))
}

fn opt_number(map: &Map<String, Value>, key: &str, default: f32) -> Parsed<f32> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => number(v, key),
    }
}

fn opt_vec3(map: &Map<String, Value>, key: &str, default: Vec3) -> Parsed<Vec3> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => vec3(v, key),
    }
}

fn tag<'a>(map: &'a Map<String, Value>, what: &str) -> Parsed<&'a str> {
    map.get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` needs a string `type`")).into())
}

fn object<'a>(value: &'a Value, what: &str) -> Parsed<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be an object")).into())
}

fn parse_falloff(value: Option<&Value>) -> Parsed<Falloff> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Falloff::None);
    };
    let map = object(value, "falloff")?;
    match tag(map, "falloff")? {
        "none" => Ok(Falloff::None),
        "linear" => {
            let range = opt_number(map, "range", 1.0)?;
            if range < 0.0 {
                return Err(Bad::Refused(
                    "invalid_value",
                    "`linear.range` cannot be negative".into(),
                ));
            }
            Ok(Falloff::Linear { range })
        }
        "inverse_square" => {
            let min_distance = opt_number(map, "min_distance", 1.0)?;
            if min_distance < 0.0 {
                return Err(Bad::Refused(
                    "invalid_value",
                    "`inverse_square.min_distance` cannot be negative".into(),
                ));
            }
            Ok(Falloff::InverseSquare { min_distance })
        }
        "curve" => {
            let points = map.get("points").and_then(Value::as_array).ok_or_else(|| {
                RpcError::invalid_params("a curve falloff needs `points`: [[distance, factor], ...]")
            })?;
            if points.is_empty() {
                return Err(Bad::Refused(
                    "invalid_value",
                    "a curve falloff needs at least one point".into(),
                ));
            }
            if points.len() > MAX_CURVE_POINTS {
                return Err(Bad::Refused(
                    "invalid_value",
                    format!("a curve takes at most {MAX_CURVE_POINTS} points"),
                ));
            }
            let mut out = Vec::with_capacity(points.len());
            for p in points {
                let pair = p
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .ok_or_else(|| RpcError::invalid_params("each curve point is [distance, factor]"))?;
                out.push((number(&pair[0], "points")?, number(&pair[1], "points")?));
            }
            if out.windows(2).any(|w| w[1].0 < w[0].0) {
                return Err(Bad::Refused(
                    "invalid_value",
                    "curve points must be sorted by distance".into(),
                ));
            }
            Ok(Falloff::Curve(out))
        }
        other => Err(Bad::Refused(
            "unknown_falloff",
            format!("`{other}` is not a falloff"),
        )),
    }
}

fn parse_volume(value: &Value) -> Parsed<FieldVolume> {
    let map = object(value, "volume")?;
    let volume = match tag(map, "volume")? {
        "global" => FieldVolume::Global,
        "sphere" => FieldVolume::Sphere {
            radius: opt_number(map, "radius", 1.0)?,
        },
        "box" => FieldVolume::Box {
            half_extents: opt_vec3(map, "half_extents", Vec3::splat(0.5))?,
        },
        "capsule" => FieldVolume::Capsule {
            half_height: opt_number(map, "half_height", 1.0)?,
            radius: opt_number(map, "radius", 0.5)?,
        },
        other => {
            return Err(Bad::Refused(
                "unknown_volume",
                format!("`{other}` is not a volume"),
            ));
        }
    };
    let sizes = match &volume {
        FieldVolume::Global => vec![],
        FieldVolume::Sphere { radius } => vec![*radius],
        FieldVolume::Box { half_extents } => vec![half_extents.x, half_extents.y, half_extents.z],
        FieldVolume::Capsule { half_height, radius } => vec![*half_height, *radius],
    };
    if sizes.iter().any(|s| *s < 0.0) {
        return Err(Bad::Refused(
            "invalid_value",
            "volume sizes cannot be negative".into(),
        ));
    }
    Ok(volume)
}

fn parse_kind(value: &Value) -> Parsed<FieldKind> {
    let map = object(value, "kind")?;
    Ok(match tag(map, "kind")? {
        "directional" => FieldKind::Directional {
            vector: opt_vec3(map, "vector", Vec3::ZERO)?,
        },
        "radial" => FieldKind::Radial {
            strength: opt_number(map, "strength", 0.0)?,
            falloff: parse_falloff(map.get("falloff"))?,
        },
        "vortex" => FieldKind::Vortex {
            tangential: opt_number(map, "tangential", 0.0)?,
            inward: opt_number(map, "inward", 0.0)?,
            axis: opt_vec3(map, "axis", Vec3::Y)?,
            max_speed: opt_number(map, "max_speed", 0.0)?,
            falloff: parse_falloff(map.get("falloff"))?,
        },
        "gravity" => FieldKind::Gravity {
            gravity: opt_vec3(map, "gravity", Vec3::new(0.0, -9.81, 0.0))?,
        },
        "linear_drag" => FieldKind::LinearDrag {
            coefficient: opt_number(map, "coefficient", 0.0)?,
        },
        "angular_drag" => FieldKind::AngularDrag {
            coefficient: opt_number(map, "coefficient", 0.0)?,
        },
        "impulse_pad" => FieldKind::ImpulsePad {
            impulse: opt_vec3(map, "impulse", Vec3::ZERO)?,
        },
        other => {
            return Err(Bad::Refused(
                "unknown_kind",
                format!("`{other}` is not a field kind"),
            ));
        }
    })
}

fn parse_mode(s: &str) -> Parsed<FieldMode> {
    match s {
        "force" => Ok(FieldMode::Force),
        "acceleration" => Ok(FieldMode::Acceleration),
        other => Err(Bad::Refused("unknown_mode", format!("`{other}` is not a mode"))),
    }
}

fn v3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn falloff_json(f: &Falloff) -> Value {
    match f {
        Falloff::None => json!({ "type": "none" }),
        Falloff::Linear { range } => json!({ "type": "linear", "range": range }),
        Falloff::InverseSquare { min_distance } => {
            json!({ "type": "inverse_square", "min_distance": min_distance })
        }
        Falloff::Curve(points) => {
            json!({ "type": "curve", "points": points.iter().map(|(d, f)| json!([d, f])).collect::<Vec<_>>() })
        }
    }
}

fn kind_json(k: &FieldKind) -> Value {
    match k {
        FieldKind::Directional { vector } => json!({ "type": "directional", "vector": v3(*vector) }),
        FieldKind::Radial { strength, falloff } => {
            json!({ "type": "radial", "strength": strength, "falloff": falloff_json(falloff) })
        }
        FieldKind::Vortex {
            tangential,
            inward,
            axis,
            max_speed,
            falloff,
        } => json!({
            "type": "vortex", "tangential": tangential, "inward": inward, "axis": v3(*axis),
            "max_speed": max_speed, "falloff": falloff_json(falloff),
        }),
        FieldKind::Gravity { gravity } => json!({ "type": "gravity", "gravity": v3(*gravity) }),
        FieldKind::LinearDrag { coefficient } => json!({ "type": "linear_drag", "coefficient": coefficient }),
        FieldKind::AngularDrag { coefficient } => {
            json!({ "type": "angular_drag", "coefficient": coefficient })
        }
        FieldKind::ImpulsePad { impulse } => json!({ "type": "impulse_pad", "impulse": v3(*impulse) }),
    }
}

fn volume_json(v: &FieldVolume) -> Value {
    match v {
        FieldVolume::Global => json!({ "type": "global" }),
        FieldVolume::Sphere { radius } => json!({ "type": "sphere", "radius": radius }),
        FieldVolume::Box { half_extents } => json!({ "type": "box", "half_extents": v3(*half_extents) }),
        FieldVolume::Capsule { half_height, radius } => {
            json!({ "type": "capsule", "half_height": half_height, "radius": radius })
        }
    }
}

fn field_json(entity: Entity, field: &ForceField, pose: &FieldPose) -> Value {
    json!({
        "entity": entity.to_bits(),
        "kind": kind_json(&field.kind),
        "volume": volume_json(&field.volume),
        "mode": match field.mode { FieldMode::Force => "force", FieldMode::Acceleration => "acceleration" },
        "mask": field.mask,
        "enabled": field.enabled,
        "priority": field.priority,
        "position": v3(pose.position),
        "rotation": [pose.rotation.x, pose.rotation.y, pose.rotation.z, pose.rotation.w],
    })
}

fn entity_of(params: &Value, required: bool) -> Result<Option<Entity>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) if !required => Ok(None),
        None | Some(Value::Null) => Err(RpcError::invalid_params("`entity` is required")),
        Some(v) => {
            let bits = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        }
    }
}

fn resolve<T>(parsed: Parsed<T>) -> Result<Result<T, Value>, RpcError> {
    match parsed {
        Ok(v) => Ok(Ok(v)),
        Err(Bad::Rpc(e)) => Err(e),
        Err(Bad::Refused(reason, detail)) => Ok(Err(failed(reason, detail))),
    }
}

/// Every field, sorted by entity, with its world pose. `entity` limits it to one.
fn collect(world: &mut World) -> Vec<(Entity, ForceField, FieldPose)> {
    let mut q = world.query::<(Entity, &ForceField, Option<&GlobalTransform>, Option<&Transform>)>();
    let mut out: Vec<_> = q
        .iter(world)
        .map(|(e, f, g, t)| (e, f.clone(), field_pose(g, t, false)))
        .collect();
    // A child's `GlobalTransform` only propagates in `PostUpdate`: compose its `Transform`
    // chain like the step (`apply_force_fields`) and `physics.field_set` do, so the listing and
    // the probe use the pose the next fixed step will use.
    for (e, _, pose) in &mut out {
        if world.get::<ChildOf>(*e).is_some() {
            *pose = world_field_pose(*e, &|x| world.get::<ChildOf>(x).map(|c| c.parent()), &|x| {
                world.get::<Transform>(x).copied()
            });
        }
    }
    // The order the physics step folds fields in (latest spawned first, ADR-1347), so the probe's
    // sums and gravity tie-break match the simulation; entity ids without the physics plugin.
    match world.get_resource::<regia_ecs::spawn_order::SpawnOrder<ForceField>>() {
        Some(order) => out.sort_by_cached_key(|(e, _, _)| std::cmp::Reverse(order.key(*e))),
        None => out.sort_by_key(|(e, _, _)| e.to_bits()),
    }
    out
}

/// `physics.fields`: every force field with its kind, volume, mode, mask, state and pose.
pub fn fields(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = entity_of(params, false)?;
    let list: Vec<Value> = collect(world)
        .iter()
        .filter(|(e, _, _)| only.is_none_or(|o| o == *e))
        .map(|(e, f, p)| field_json(*e, f, p))
        .collect();
    if let Some(o) = only
        && list.is_empty()
    {
        return Err(RpcError::not_found(
            format!("entity {} has no ForceField", o.to_bits()),
            "call physics.fields without `entity` to list them, or physics.field_set to add one",
        ));
    }
    let pad_entries = world
        .get_resource::<regia_physics::FieldScratch>()
        .map_or(0, |s| s.pad_entries);
    Ok(json!({ "count": list.len(), "pad_entries": pad_entries, "fields": list }))
}

/// `physics.field_set`: create (no `entity`, or an entity without a field; `kind` is required),
/// edit (any of `kind`, `volume`, `mode`, `mask`, `enabled`, `priority`, `position`, `rotation`)
/// or remove (`remove: true`) a force field.
pub fn field_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_of(params, false)?;
    let map = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    if let Some(e) = entity
        && world.get_entity(e).is_err()
    {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", e.to_bits()),
            "list entities with scene.query_entities",
        ));
    }
    if opt_bool(map, "remove")? == Some(true) {
        let Some(e) = entity else {
            return Err(RpcError::invalid_params("`remove` needs an `entity`"));
        };
        if world.entity_mut(e).take::<ForceField>().is_none() {
            return Ok(failed(
                "no_field",
                format!("entity {} has no ForceField", e.to_bits()),
            ));
        }
        return Ok(json!({ "entity": e.to_bits(), "removed": true }));
    }

    let kind = match map.get("kind") {
        Some(v) => match resolve(parse_kind(v))? {
            Ok(k) => Some(k),
            Err(refused) => return Ok(refused),
        },
        None => None,
    };
    let volume = match map.get("volume") {
        Some(v) => match resolve(parse_volume(v))? {
            Ok(k) => Some(k),
            Err(refused) => return Ok(refused),
        },
        None => None,
    };
    let mode = match opt_str(map, "mode")? {
        Some(s) => match resolve(parse_mode(s))? {
            Ok(m) => Some(m),
            Err(refused) => return Ok(refused),
        },
        None => None,
    };
    let position = match map.get("position") {
        Some(v) => match resolve(vec3(v, "position"))? {
            Ok(p) => Some(p),
            Err(refused) => return Ok(refused),
        },
        None => None,
    };
    let rotation = match map.get("rotation") {
        Some(v) => {
            let Some(a) = v.as_array().filter(|a| a.len() == 4) else {
                return Err(RpcError::invalid_params("`rotation` must be [x, y, z, w]"));
            };
            let mut q = [0.0f32; 4];
            for (slot, item) in q.iter_mut().zip(a) {
                match resolve(number(item, "rotation"))? {
                    Ok(n) => *slot = n,
                    Err(refused) => return Ok(refused),
                }
            }
            let quat = Quat::from_xyzw(q[0], q[1], q[2], q[3]);
            if quat.length_squared() < 1e-12 {
                return Ok(failed(
                    "invalid_value",
                    "`rotation` cannot be the zero quaternion",
                ));
            }
            Some(quat.normalize())
        }
        None => None,
    };
    let mask = match opt_u64(map, "mask")? {
        Some(m) => match u32::try_from(m) {
            Ok(m) => Some(m),
            Err(_) => {
                return Ok(failed(
                    "invalid_value",
                    "`mask` must be an integer from 0 to 4294967295",
                ));
            }
        },
        None => None,
    };
    let enabled = opt_bool(map, "enabled")?;
    let priority = match opt_i64(map, "priority")? {
        Some(p) => match i32::try_from(p) {
            Ok(p) => Some(p),
            Err(_) => {
                return Ok(failed(
                    "invalid_value",
                    "`priority` must be an integer from -2147483648 to 2147483647",
                ));
            }
        },
        None => None,
    };

    let (target, created) = match entity {
        Some(e) => (e, world.get::<ForceField>(e).is_none()),
        None => (world.spawn(Transform::IDENTITY).id(), true),
    };
    let mut field = world.get::<ForceField>(target).cloned();
    if field.is_none() {
        let Some(kind) = kind.clone() else {
            if entity.is_none() {
                world.despawn(target);
            }
            return Ok(failed("missing_kind", "creating a field needs `kind`"));
        };
        field = Some(ForceField::new(kind, FieldVolume::Global));
    }
    let mut field = field.unwrap_or_default();
    if let Some(k) = kind {
        field.kind = k;
    }
    if let Some(v) = volume {
        field.volume = v;
    }
    if let Some(m) = mode {
        field.mode = m;
    }
    if let Some(m) = mask {
        field.mask = m;
    }
    if let Some(e) = enabled {
        field.enabled = e;
    }
    if let Some(p) = priority {
        field.priority = p;
    }
    // `position`/`rotation` are world space. A root's Transform is its world pose; a child's
    // Transform is local to its parent's composed world pose, so convert.
    let is_child = world.get::<ChildOf>(target).is_some();
    let local_pose = if position.is_some() || rotation.is_some() {
        Some(if is_child {
            let current = world_field_pose(target, &|e| world.get::<ChildOf>(e).map(|c| c.parent()), &|e| {
                world.get::<Transform>(e).copied()
            });
            let desired = FieldPose {
                position: position.unwrap_or(current.position),
                rotation: rotation.unwrap_or(current.rotation),
            };
            let parent =
                ancestor_world_affine(target, &|e| world.get::<ChildOf>(e).map(|c| c.parent()), &|e| {
                    world.get::<Transform>(e).copied()
                });
            localize_under_parent(desired, parent)
        } else {
            let mut t = world
                .get::<Transform>(target)
                .copied()
                .unwrap_or(Transform::IDENTITY);
            if let Some(p) = position {
                t.translation = p;
            }
            if let Some(r) = rotation {
                t.rotation = r;
            }
            (t.translation, t.rotation)
        })
    } else {
        None
    };
    let mut entity_mut = world.entity_mut(target);
    entity_mut.insert(field);
    if let Some((translation, rotation)) = local_pose {
        let mut t = entity_mut
            .get::<Transform>()
            .copied()
            .unwrap_or(Transform::IDENTITY);
        t.translation = translation;
        t.rotation = rotation;
        entity_mut.insert(t);
    }
    let field = world.get::<ForceField>(target).cloned().unwrap_or_default();
    // The reported pose is world space: composed from the Transform chain for a child (current
    // this fixed step), not the propagated GlobalTransform.
    let pose = if is_child {
        world_field_pose(target, &|e| world.get::<ChildOf>(e).map(|c| c.parent()), &|e| {
            world.get::<Transform>(e).copied()
        })
    } else {
        field_pose(
            world.get::<GlobalTransform>(target),
            world.get::<Transform>(target),
            false,
        )
    };
    let mut out = field_json(target, &field, &pose);
    out["created"] = json!(created);
    Ok(out)
}

/// `physics.field_probe`: the force a mass would feel at `point`, field by field.
pub fn field_probe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    let point = match map.get("point") {
        Some(v) => match resolve(vec3(v, "point"))? {
            Ok(p) => p,
            Err(refused) => return Ok(refused),
        },
        None => return Err(RpcError::invalid_params("`point` ([x, y, z]) is required")),
    };
    let get = |key: &str, default: Vec3| match map.get(key) {
        None | Some(Value::Null) => resolve(Ok(default)),
        Some(v) => resolve(vec3(v, key)),
    };
    let velocity = match get("velocity", Vec3::ZERO)? {
        Ok(v) => v,
        Err(refused) => return Ok(refused),
    };
    let angular_velocity = match get("angular_velocity", Vec3::ZERO)? {
        Ok(v) => v,
        Err(refused) => return Ok(refused),
    };
    let mass = match map.get("mass") {
        None | Some(Value::Null) => 1.0,
        Some(v) => match resolve(number(v, "mass"))? {
            Ok(m) => m,
            Err(refused) => return Ok(refused),
        },
    };
    if mass <= 0.0 {
        return Ok(failed("invalid_value", "`mass` must be positive"));
    }
    let layer = match opt_u64(map, "layer")? {
        None => 0u16,
        Some(layer) if layer < 32 => layer as u16,
        Some(_) => {
            return Ok(failed("invalid_value", "`layer` must be an integer from 0 to 31"));
        }
    };
    let gravity = world
        .get_resource::<regia_scene::level::LevelSettings>()
        .map(|l| l.gravity)
        .or_else(|| {
            world
                .get_resource::<regia_physics::PhysicsSettings>()
                .map(|s| s.gravity)
        })
        .unwrap_or(Vec3::new(0.0, -9.81, 0.0));
    let sample = BodySample {
        position: point,
        velocity,
        angular_velocity,
        mass,
        inertia: mass * 0.4,
    };
    let all = collect(world);
    let probe = probe_fields(all.iter().map(|(e, f, p)| (*e, f, *p)), &sample, layer, gravity);
    Ok(json!({
        "point": v3(point),
        "mass": mass,
        "force": v3(probe.force),
        "torque": v3(probe.torque),
        "acceleration": v3(probe.force / mass),
        "gravity_override_force": probe.gravity.map(v3),
        "entry_impulse": v3(probe.impulse),
        "fields": probe.per_field.iter().map(|(e, f)| json!({ "entity": e.to_bits(), "force": v3(*f) })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::prelude::ChildOf;

    #[test]
    fn set_list_probe_remove_round_trip() {
        let mut world = World::new();
        let made = field_set(
            &mut world,
            &json!({
                "kind": { "type": "radial", "strength": 40.0, "falloff": { "type": "inverse_square", "min_distance": 0.1 } },
                "volume": { "type": "sphere", "radius": 10.0 },
                "position": [0.0, 0.0, 0.0],
            }),
        )
        .expect("set");
        assert_eq!(made["created"], true);
        let entity = made["entity"].as_u64().expect("id");
        let list = fields(&mut world, &json!({})).expect("list");
        assert_eq!(list["count"], 1);
        assert_eq!(list["fields"][0]["kind"]["type"], "radial");
        let probe =
            field_probe(&mut world, &json!({ "point": [2.0, 0.0, 0.0], "mass": 2.0 })).expect("probe");
        let fx = probe["force"][0].as_f64().expect("fx");
        assert!(
            (fx + 10.0).abs() < 1e-4,
            "40 / 4 = 10 N toward the center, got {fx}"
        );
        // Edit: disable, then probe again.
        let edit = field_set(&mut world, &json!({ "entity": entity, "enabled": false })).expect("edit");
        assert_eq!(edit["created"], false);
        let off = field_probe(&mut world, &json!({ "point": [2.0, 0.0, 0.0] })).expect("probe");
        assert_eq!(off["force"][0], 0.0);
        let gone = field_set(&mut world, &json!({ "entity": entity, "remove": true })).expect("remove");
        assert_eq!(gone["removed"], true);
        let again =
            field_set(&mut world, &json!({ "entity": entity, "remove": true })).expect("remove again");
        assert_eq!(again["reason"], "no_field");
        assert!(fields(&mut world, &json!({ "entity": entity })).is_err());
    }

    #[test]
    fn the_probe_breaks_gravity_ties_in_spawn_order_like_the_step() {
        // Corr-physics audit: the probe and the step sorted fields by entity id, which parallel
        // spawning makes thread-timing dependent. Both now fold the latest arrived field first.
        // The first field goes on the lower id, so by id it would fold first and win.
        let mut world = World::new();
        regia_ecs::spawn_order::try_track_spawn_order::<ForceField>(&mut world).expect("tracking");
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let (lower, higher) = if a.to_bits() < b.to_bits() { (a, b) } else { (b, a) };
        world.entity_mut(lower).insert(ForceField::new(
            FieldKind::Gravity { gravity: Vec3::X },
            FieldVolume::Global,
        ));
        world.entity_mut(higher).insert(ForceField::new(
            FieldKind::Gravity { gravity: Vec3::Z },
            FieldVolume::Global,
        ));
        let probe =
            field_probe(&mut world, &json!({ "point": [0.0, 0.0, 0.0], "mass": 1.0 })).expect("probe");
        let g = &probe["gravity_override_force"];
        assert!(
            g[2].as_f64().expect("z") > 0.5 && g[0].as_f64().expect("x").abs() < 1e-6,
            "the latest field wins: {g}"
        );
        let list = fields(&mut world, &json!({})).expect("list");
        assert_eq!(
            list["fields"][0]["entity"].as_u64(),
            Some(higher.to_bits()),
            "listed in fold order"
        );
    }

    #[test]
    fn refusals_carry_lowercase_reasons() {
        let mut world = World::new();
        let reason = |v: Value| v["reason"].as_str().map(str::to_owned);
        let r = field_set(&mut world, &json!({ "kind": { "type": "teleporter" } })).expect("ok");
        assert_eq!(reason(r).as_deref(), Some("unknown_kind"));
        let r = field_set(
            &mut world,
            &json!({ "volume": { "type": "sphere", "radius": 1.0 } }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("missing_kind"));
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "directional" }, "volume": { "type": "cone" } }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("unknown_volume"));
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "radial", "falloff": { "type": "curve", "points": [[2.0, 1.0], [1.0, 0.0]] } } }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "directional" }, "mode": "weight" }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("unknown_mode"));
        let r = field_probe(&mut world, &json!({ "point": [0, 0, 0], "mass": -1.0 })).expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        assert!(field_probe(&mut world, &json!({})).is_err());
        // Out-of-range integers are refused, not truncated or clamped.
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "directional" }, "mask": 4294967297u64 }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "directional" }, "priority": 3000000000i64 }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        let r = field_probe(&mut world, &json!({ "point": [0, 0, 0], "layer": 70000 })).expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        // An empty curve and negative falloff distances are refused.
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "radial", "strength": 1.0, "falloff": { "type": "curve", "points": [] } } }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "radial", "strength": 1.0, "falloff": { "type": "linear", "range": -1.0 } } }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "radial", "strength": 1.0, "falloff": { "type": "inverse_square", "min_distance": -1.0 } } }),
        )
        .expect("ok");
        assert_eq!(reason(r).as_deref(), Some("invalid_value"));
        // Nothing was left behind by the refused creations.
        assert_eq!(fields(&mut world, &json!({})).expect("list")["count"], 0);
    }

    #[test]
    fn field_set_rejects_a_mask_or_priority_that_does_not_fit() {
        let mut world = World::new();
        let made = field_set(
            &mut world,
            &json!({
                "kind": { "type": "directional", "vector": [0.0, 1.0, 0.0] },
                "mask": 7,
                "priority": 4,
            }),
        )
        .expect("create");
        let entity = made["entity"].as_u64().expect("id");
        let reason = |v: &Value| v.get("reason").and_then(Value::as_str).map(str::to_owned);

        let r = field_set(&mut world, &json!({ "entity": entity, "mask": 4_294_967_296u64 })).expect("mask");
        assert_eq!(reason(&r).as_deref(), Some("invalid_value"), "{r}");
        let listed = fields(&mut world, &json!({})).expect("list");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["fields"][0]["mask"], json!(7));
        assert_eq!(listed["fields"][0]["priority"], json!(4));

        let r = field_set(
            &mut world,
            &json!({ "entity": entity, "priority": 2_147_483_648i64 }),
        )
        .expect("priority high");
        assert_eq!(reason(&r).as_deref(), Some("invalid_value"), "{r}");
        let r = field_set(
            &mut world,
            &json!({ "entity": entity, "priority": -2_147_483_649i64 }),
        )
        .expect("priority low");
        assert_eq!(reason(&r).as_deref(), Some("invalid_value"), "{r}");
        let listed = fields(&mut world, &json!({})).expect("list");
        assert_eq!(listed["fields"][0]["mask"], json!(7));
        assert_eq!(listed["fields"][0]["priority"], json!(4));

        let r = field_set(
            &mut world,
            &json!({ "kind": { "type": "directional" }, "mask": 4_294_967_296u64 }),
        )
        .expect("create overflow");
        assert_eq!(reason(&r).as_deref(), Some("invalid_value"), "{r}");
        assert_eq!(fields(&mut world, &json!({})).expect("list")["count"], 1);
    }

    #[test]
    fn field_set_stores_mask_and_priority_at_the_integer_limits() {
        let mut world = World::new();
        let made = field_set(
            &mut world,
            &json!({
                "kind": { "type": "directional", "vector": [0.0, 1.0, 0.0] },
                "mask": 4_294_967_295u64,
                "priority": 2_147_483_647i64,
            }),
        )
        .expect("set");
        assert!(made.get("reason").is_none(), "{made}");
        assert_eq!(made["mask"], json!(4_294_967_295u64));
        assert_eq!(made["priority"], json!(2_147_483_647i64));
        let low = field_set(
            &mut world,
            &json!({ "entity": made["entity"].as_u64().expect("id"), "priority": -2_147_483_648i64 }),
        )
        .expect("edit");
        assert_eq!(low["priority"], json!(-2_147_483_648i64));
        assert_eq!(low["mask"], json!(4_294_967_295u64));
    }

    #[test]
    fn field_probe_rejects_a_collision_layer_past_31() {
        let mut world = World::new();
        field_set(
            &mut world,
            &json!({ "kind": { "type": "directional", "vector": [0.0, 30.0, 0.0] } }),
        )
        .expect("field");
        let ok =
            field_probe(&mut world, &json!({ "point": [0.0, 0.0, 0.0], "layer": 31 })).expect("layer 31");
        assert!(ok.get("reason").is_none(), "{ok}");
        assert!(ok.get("force").is_some(), "{ok}");
        for layer in [32_u64, 40] {
            let bad =
                field_probe(&mut world, &json!({ "point": [0.0, 0.0, 0.0], "layer": layer })).expect("probe");
            assert_eq!(bad["reason"], "invalid_value", "layer {layer}: {bad}");
        }
    }

    #[test]
    fn field_set_position_is_world_space_even_for_a_child() {
        let mut world = World::new();
        let parent = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        // A stale propagated pose: the child's true composed pose is (11, 0, 0).
        let child = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                ChildOf(parent),
                GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    99.0, 0.0, 0.0,
                ))),
            ))
            .id();
        let made = field_set(
            &mut world,
            &json!({
                "entity": child,
                "kind": { "type": "directional", "vector": [1.0, 0.0, 0.0] },
                "position": [2.0, 0.0, 0.0],
            }),
        )
        .expect("set");
        assert_eq!(made["created"], true);
        assert_eq!(
            made["position"].as_array().expect("position")[0]
                .as_f64()
                .expect("x"),
            2.0,
            "the reported pose is the composed world pose, not the stale global"
        );
        // The local transform holds the position relative to the parent.
        let local = world.get::<Transform>(child).expect("the child's transform");
        assert!(
            (local.translation.x - -8.0).abs() < 1e-6,
            "local x = world - parent: {:?}",
            local.translation
        );
    }

    /// Review rev-kimi-20 finding 3: `physics.fields` and `physics.field_probe` place a child
    /// field at its composed `Transform` chain pose, like the simulation and `physics.field_set`,
    /// not at the `GlobalTransform` that only propagates in `PostUpdate`.
    #[test]
    fn fields_and_probe_place_a_child_field_at_its_composed_pose() {
        let mut world = World::new();
        let parent = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        // The child's composed pose is (11, 0, 0); the propagated one is stale at 99.
        let child = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                ChildOf(parent),
                GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
                    99.0, 0.0, 0.0,
                ))),
            ))
            .id();
        let made = field_set(
            &mut world,
            &json!({
                "entity": child,
                "kind": { "type": "directional", "vector": [1.0, 0.0, 0.0] },
                "volume": { "type": "sphere", "radius": 1.0 },
                "position": [2.0, 0.0, 0.0],
            }),
        )
        .expect("set");
        assert_eq!(made["created"], true);
        let list = fields(&mut world, &json!({ "entity": child })).expect("list");
        let x = list["fields"][0]["position"][0].as_f64().expect("x");
        assert!(
            (x - 2.0).abs() < 1e-5,
            "physics.fields reports the composed pose (2), not the stale global: {x}"
        );
        let probe = field_probe(&mut world, &json!({ "point": [2.0, 0.0, 0.0] })).expect("probe");
        let fx = probe["force"][0].as_f64().expect("fx");
        assert!(
            (fx - 1.0).abs() < 1e-5,
            "the probe at the field's composed position feels it: {probe}"
        );
        let stale = field_probe(&mut world, &json!({ "point": [99.0, 0.0, 0.0] })).expect("probe");
        assert_eq!(stale["force"][0], 0.0, "nothing at the stale pose: {stale}");
    }
}
