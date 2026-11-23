// SPDX-License-Identifier: MPL-2.0
//! `anim.node_drivers`: drives and current angles on plain nodes (ADR-8410).

use regia_animation::node_driver::{
    DriveKind, NodeDrive, NodeDriveAngles, NodeDriveParams, NodeDriveState, NodeDriver, slew_pitch_param,
};
use regia_ecs::prelude::{Entity, World};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Drives and current angles. With no `entity`, every entity that has a `NodeDriver`.
/// With `entity`, that one entity.
///
/// # Errors
///
/// `F0002` when `entity` is present and not an integer id. `F0003` when that entity is missing
/// or has no `NodeDriver`.
pub fn node_drivers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    match entity_param(params)? {
        Some(entity) => Ok(one(world, entity)?),
        None => Ok(json!({ "entities": list(world) })),
    }
}

fn entity_param(params: &Value) -> Result<Option<Entity>, RpcError> {
    let Some(value) = params.get("entity") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(bits) = value.as_u64() else {
        return Err(RpcError::invalid_params("`entity` must be an integer id"));
    };
    Entity::try_from_bits(bits)
        .map(Some)
        .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

fn list(world: &mut World) -> Vec<Value> {
    let mut query = world.query::<(Entity, &NodeDriver)>();
    let ids: Vec<Entity> = query.iter(world).map(|(entity, _)| entity).collect();
    ids.into_iter()
        .filter_map(|entity| one(world, entity).ok())
        .collect()
}

fn one(world: &World, entity: Entity) -> Result<Value, RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            "no such entity",
            "pass an id from scene.query_entities",
        ));
    }
    let Some(driver) = world.get::<NodeDriver>(entity) else {
        return Err(RpcError::not_found(
            "entity has no node driver",
            "insert a NodeDriver on the entity",
        ));
    };
    let params = world.get::<NodeDriveParams>(entity).cloned().unwrap_or_default();
    let angles = world
        .get::<NodeDriveState>(entity)
        .map(|state| state.angles.clone())
        .unwrap_or_default();
    Ok(json!({
        "entity": entity.to_bits(),
        "drives": drive_list(driver, &angles, &params),
        "params": param_list(&params),
    }))
}

fn param_list(params: &NodeDriveParams) -> Vec<Value> {
    params
        .values
        .iter()
        .map(|param| json!({ "name": param.name, "value": num(param.value) }))
        .collect()
}

fn drive_list(driver: &NodeDriver, angles: &[NodeDriveAngles], params: &NodeDriveParams) -> Vec<Value> {
    driver
        .drives
        .iter()
        .enumerate()
        .map(|(index, drive)| drive_json(drive, angles.get(index).copied().unwrap_or_default(), params))
        .collect()
}

fn drive_json(drive: &NodeDrive, angles: NodeDriveAngles, params: &NodeDriveParams) -> Value {
    let mut row = json!({
        "node": drive.node,
        "input": drive.input,
        "primary": num(angles.primary),
        "secondary": num(angles.secondary),
        "value": num(params.get(&drive.input)),
    });
    match &drive.kind {
        DriveKind::Hinge { axis, min, max, rate } => {
            row["kind"] = json!("hinge");
            row["axis"] = vec3(*axis);
            row["min"] = num(*min);
            row["max"] = num(*max);
            row["rate"] = num(*rate);
        }
        DriveKind::Spin { axis } => {
            row["kind"] = json!("spin");
            row["axis"] = vec3(*axis);
        }
        DriveKind::Slew {
            yaw_axis,
            pitch_axis,
            max_rate,
            limits,
        } => {
            let pitch = slew_pitch_param(&drive.input);
            row["kind"] = json!("slew");
            row["yaw_axis"] = vec3(*yaw_axis);
            row["pitch_axis"] = vec3(*pitch_axis);
            row["max_rate"] = num(*max_rate);
            row["limits"] = json!({
                "yaw_min": num(limits.yaw_min),
                "yaw_max": num(limits.yaw_max),
                "pitch_min": num(limits.pitch_min),
                "pitch_max": num(limits.pitch_max),
            });
            row["pitch_value"] = num(params.get(&pitch));
        }
    }
    row
}

fn vec3(axis: regia_core::math::Vec3) -> Value {
    json!([num(axis.x), num(axis.y), num(axis.z)])
}

fn num(value: f32) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}
