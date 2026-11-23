// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the data-driven camera framework (spec 35, book/agents/camera-framework.md):
//! `camera.assets`, `camera.validate`, `camera.inspect`, `camera.set_var`, `camera.set_param`,
//! `camera.set_input`, `camera.push_overlay`, `camera.add_additive`, `camera.release_layer`,
//! `camera.hold_overlay`, `camera.set_driver`, `camera.load`.
//!
//! Handlers are plain functions over `&mut World`. A world without `CameraPlugin` answers
//! `UNSUPPORTED`. Errors carry `data.reason`, one of the lowercase codes listed on the book page
//! (`camera_no_driver`, `camera_unknown_asset`, `camera_unknown_rig`, `camera_unknown_hook`,
//! `camera_unknown_name`, `camera_bad_value`, `camera_unknown_layer_id`, `camera_order_taken`,
//! `camera_asset_invalid`, `camera_bad_path`, `camera_file_not_found`).

use regia_app::prelude::*;
use regia_framework::camera::asset::{
    Blend, CameraBuildStatus, CameraError, CameraIssue, ParamValue, RigMatch, StartFacing, TargetRef,
    TransitionDef, VarValue,
};
use regia_framework::camera::director::CameraHooks;
use regia_framework::camera::driver::{CameraAssets, CameraDriver};
use regia_framework::camera::load::{self, CameraFiles};
use regia_framework::camera::nodes::CameraPose;
use regia_framework::camera::runtime::{DriverCore, LayerId, OverlayOptions, ReleaseBy};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn unsupported() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0016",
        "no camera framework in this app",
        "add regia_framework::camera::CameraPlugin",
    )
}

/// An error carrying `data.reason`.
pub(crate) fn refused(
    code: i64,
    regia_code: &str,
    reason: &str,
    message: impl Into<String>,
    hint: &str,
) -> RpcError {
    let mut error = RpcError::new(code, regia_code, message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

pub(crate) fn invalid(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    refused(protocol::INVALID_PARAMS, "F0002", reason, message, hint)
}

pub(crate) fn missing(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    refused(protocol::NOT_FOUND, "F0003", reason, message, hint)
}

/// The protocol error for a framework error.
pub(crate) fn framework_error(error: CameraError) -> RpcError {
    let reason = error.code();
    match &error {
        CameraError::UnknownRig(_) => missing(
            reason,
            error.to_string(),
            "list the rigs with camera.assets, or the active ones with camera.inspect",
        ),
        CameraError::UnknownLayer(_) => missing(
            reason,
            error.to_string(),
            "camera.inspect lists the live layers and their ids",
        ),
        CameraError::UnknownName(_) => missing(
            reason,
            error.to_string(),
            "camera.inspect lists the variables and input slots; camera.assets lists the parameters",
        ),
        CameraError::OrderTaken(_) => invalid(
            reason,
            error.to_string(),
            "pick another order or pass allow_same_order: true",
        ),
        CameraError::BadParam { .. } | CameraError::BadValue(_) => invalid(
            reason,
            error.to_string(),
            "send a value of the declared type with finite numbers",
        ),
        CameraError::Invalid(_)
        | CameraError::Parse(_)
        | CameraError::Version { .. }
        | CameraError::Migration(_) => invalid(
            reason,
            error.to_string(),
            "the driver's asset is not loaded or has errors; camera.validate lists them",
        ),
    }
}

pub(crate) fn require_framework(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<CameraAssets>().is_none() {
        return Err(unsupported());
    }
    Ok(())
}

pub(crate) fn entity_param(params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .ok_or_else(|| {
            RpcError::invalid_params("`entity` (the camera's id from scene.query_entities) is required")
        })?
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
    Entity::try_from_bits(bits).ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
}

fn str_param<'a>(params: &'a Value, field: &str) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{field}` (a string) is required")))
}

/// Reads the optional `context` parameter: a string names the entity (its `Name`), an array of
/// three numbers is a world point, and `null` or a missing field leaves the driver's target.
fn context_param(params: &Value) -> Result<Option<TargetRef>, RpcError> {
    match params.get("context") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => Ok(Some(TargetRef::Entity(name.clone()))),
        Some(Value::Array(items)) if items.len() == 3 => {
            let mut point = [0.0_f32; 3];
            for (slot, item) in point.iter_mut().zip(items) {
                let value = item
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| RpcError::invalid_params("`context` point needs 3 finite numbers"))?
                    as f32;
                // The f64 check above is not enough: a huge finite f64 casts to an infinite
                // f32, and the driver would then aim the layer at an infinite target.
                if !value.is_finite() {
                    return Err(RpcError::invalid_params("`context` point needs 3 finite numbers"));
                }
                *slot = value;
            }
            Ok(Some(TargetRef::Point(point)))
        }
        Some(_) => Err(RpcError::invalid_params(
            "`context` must be an entity name (string) or a world point ([x, y, z])",
        )),
    }
}

fn opt_bool(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a boolean"))),
    }
}

/// The driver on `entity` or `camera_no_driver`.
pub(crate) fn driver_mut(world: &mut World, entity: Entity) -> Result<Mut<'_, CameraDriver>, RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "scene.query_entities lists live entities",
        ));
    }
    world.get_mut::<CameraDriver>(entity).ok_or_else(|| {
        missing(
            "camera_no_driver",
            format!("entity {} has no CameraDriver", entity.to_bits()),
            "attach one with camera.set_driver { entity, asset }",
        )
    })
}

pub(crate) fn bound_core(driver: &mut CameraDriver) -> Result<&mut DriverCore, RpcError> {
    driver.bound_mut().map_err(framework_error)
}

fn pose_json(pose: &CameraPose) -> Value {
    json!({
        "position": pose.position.to_array(),
        "yaw": pose.yaw,
        "pitch": pose.pitch,
        "roll": pose.roll,
        "fov_deg": pose.fov_deg,
        "aim": pose.aim.map(|a| a.to_array()),
        "cut": pose.cut,
    })
}

fn issue_json(issue: &CameraIssue) -> Value {
    json!({
        "code": issue.code,
        "severity": issue.severity.name(),
        "rig": issue.rig,
        "node": issue.node,
        "message": issue.message,
    })
}

fn param_json(value: &ParamValue) -> Value {
    match value {
        ParamValue::Bool(v) => json!(v),
        ParamValue::Int(v) => json!(v),
        ParamValue::Float(v) => json!(v),
        ParamValue::Vec3(v) => json!(v),
        ParamValue::Name(v) => json!(v),
    }
}

fn var_json(value: &VarValue) -> Value {
    match value {
        VarValue::Bool(v) => json!(v),
        VarValue::Float(v) => json!(v),
        VarValue::Vec3(v) => json!(v),
        VarValue::YawPitch(yaw, pitch) => json!({ "yaw": yaw, "pitch": pitch }),
    }
}

fn numbers<const N: usize>(value: &Value) -> Option<[f32; N]> {
    let items = value.as_array().filter(|a| a.len() == N)?;
    let mut out = [0.0f32; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64()? as f32;
    }
    out.iter().all(|v| v.is_finite()).then_some(out)
}

/// A JSON value as a variable of the same kind as `like`.
fn var_from_json(like: &VarValue, value: &Value) -> Result<VarValue, CameraError> {
    let bad = || CameraError::BadValue(format!("expected a {} value", like.kind()));
    match like {
        VarValue::Bool(_) => value.as_bool().map(VarValue::Bool).ok_or_else(bad),
        VarValue::Float(_) => value
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| VarValue::Float(v as f32))
            .ok_or_else(bad),
        VarValue::Vec3(_) => numbers::<3>(value).map(VarValue::Vec3).ok_or_else(bad),
        VarValue::YawPitch(..) => {
            if let Some([yaw, pitch]) = numbers::<2>(value) {
                Ok(VarValue::YawPitch(yaw, pitch))
            } else {
                let yaw = value.get("yaw").and_then(Value::as_f64);
                let pitch = value.get("pitch").and_then(Value::as_f64);
                match (yaw, pitch) {
                    (Some(y), Some(p)) if y.is_finite() && p.is_finite() => {
                        Ok(VarValue::YawPitch(y as f32, p as f32))
                    }
                    _ => Err(bad()),
                }
            }
        }
    }
}

/// A JSON value as a parameter of the same kind as `like`.
fn param_from_json(like: &ParamValue, value: &Value) -> Result<ParamValue, CameraError> {
    let bad = || CameraError::BadValue(format!("expected a {} value", like.kind()));
    match like {
        ParamValue::Bool(_) => value.as_bool().map(ParamValue::Bool).ok_or_else(bad),
        ParamValue::Int(_) => value.as_i64().map(ParamValue::Int).ok_or_else(bad),
        ParamValue::Float(_) => value
            .as_f64()
            .filter(|v| v.is_finite())
            .map(|v| ParamValue::Float(v as f32))
            .ok_or_else(bad),
        ParamValue::Vec3(_) => numbers::<3>(value).map(ParamValue::Vec3).ok_or_else(bad),
        ParamValue::Name(_) => value
            .as_str()
            .map(|s| ParamValue::Name(s.to_owned()))
            .ok_or_else(bad),
    }
}

/// `camera.assets {}`: every loaded camera asset with its rigs, parameters and build status.
pub fn camera_assets(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let assets = world.resource::<CameraAssets>();
    let listed: Vec<Value> = assets
        .names()
        .map(|name| {
            let working = assets.working(name);
            let issues = assets.issues(name).unwrap_or(&[]);
            json!({
                "name": name,
                "path": assets.path(name),
                "rigs": working.map(|a| a.rigs.iter().map(|r| r.name.clone()).collect::<Vec<_>>()).unwrap_or_default(),
                "params": working.map(|a| a.params.iter().map(|(k, v)| (k.clone(), json!({ "type": v.kind(), "default": param_json(v) }))).collect::<serde_json::Map<_, _>>()).unwrap_or_default(),
                "status": assets.status(name).map_or("unknown", CameraBuildStatus::name),
                "issue_count": issues.len(),
                "active": assets.active(name).is_some(),
                "revision": assets.revision(name),
            })
        })
        .collect();
    let files: Vec<Value> = world
        .get_resource::<CameraFiles>()
        .into_iter()
        .flat_map(CameraFiles::iter)
        .map(|(path, file)| {
            json!({
                "path": path,
                "asset": file.asset,
                "cooked": file.cooked,
                "generation": file.generation,
                "error": file.error.as_ref().map(|e| json!({ "reason": e.reason, "detail": e.detail })),
            })
        })
        .collect();
    Ok(json!({ "count": listed.len(), "assets": listed, "files": files }))
}

/// `camera.load { path }`: reads a `.camera.ron` (its source text, else its cooked entry) into the
/// registry. Validation errors are reported, not refused: drivers stay on the last good version.
pub fn camera_load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let path = str_param(params, "path")?.to_owned();
    let status = load::load_camera_file(world, &path).map_err(|error| match error.reason {
        load::FILE_NOT_FOUND => missing(
            error.reason,
            error.detail,
            "the path is relative to assets/; assets.list shows what exists",
        ),
        _ => invalid(
            error.reason,
            error.detail,
            "a plain path to a .camera.ron under assets/, in a file that parses",
        ),
    })?;
    let path = load::normalize_camera_path(&path).map_err(|e| RpcError::internal(e.to_string()))?;
    let files = world.resource::<CameraFiles>();
    let file = files.get(&path);
    let asset = file.and_then(|file| file.asset.clone());
    let issues = asset
        .as_deref()
        .and_then(|name| world.resource::<CameraAssets>().issues(name))
        .map(|issues| issues.iter().map(issue_json).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(json!({
        "path": path,
        "asset": asset,
        "status": status.name(),
        "cooked": file.is_some_and(|file| file.cooked),
        "generation": file.map_or(0, |file| file.generation),
        "issues": issues,
    }))
}

/// `camera.validate { asset }`: validates the working copy of an asset (pure; it does not change
/// the registry).
pub fn camera_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let name = str_param(params, "asset")?;
    let assets = world.resource::<CameraAssets>();
    let Some(working) = assets.working(name) else {
        return Err(missing(
            "camera_unknown_asset",
            format!("no camera asset named {name:?}"),
            "camera.assets lists the loaded assets",
        ));
    };
    let hooks = world.get_resource::<CameraHooks>();
    let issues = working.validate(&|hook| hooks.is_some_and(|h| h.contains(hook)));
    Ok(json!({
        "asset": name,
        "status": CameraBuildStatus::of(&issues).name(),
        "issues": issues.iter().map(issue_json).collect::<Vec<_>>(),
    }))
}

/// `camera.inspect { entity }`: the driver's state, with the node-by-node pose trace of every
/// active rig.
pub fn camera_inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let driver = driver_mut(world, entity)?;
    let asset_name = driver.asset.clone();
    let enabled = driver.enabled;
    let target = driver.target.map(|t| t.to_bits());
    let Some(core) = driver.core() else {
        return Ok(json!({
            "entity": entity.to_bits(),
            "asset": asset_name,
            "enabled": enabled,
            "bound": false,
        }));
    };
    let rigs: Vec<Value> = core
        .rig_infos()
        .iter()
        .map(|rig| {
            json!({
                "name": rig.name,
                "weight": rig.weight,
                "elapsed": rig.elapsed,
                "skipped": rig.skipped,
                "pose": pose_json(&rig.pose),
                "trace": rig.trace.iter().map(|t| json!({
                    "index": t.index,
                    "node": t.kind,
                    "skipped": t.skipped,
                    "position": t.pose.position.to_array(),
                    "yaw": t.pose.yaw,
                    "pitch": t.pose.pitch,
                    "roll": t.pose.roll,
                    "fov_deg": t.pose.fov_deg,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let layers: Vec<Value> = core
        .layer_infos()
        .iter()
        .map(|l| {
            json!({
                "id": l.id.0,
                "rig": l.rig,
                "kind": l.kind,
                "order": l.order,
                "phase": l.phase,
                "weight": l.weight,
                "held": l.held,
            })
        })
        .collect();
    let transition = core.blend_info().map(|(from, to, elapsed, duration)| {
        json!({ "from": from, "to": to, "elapsed": elapsed, "duration": duration })
    });
    let variables: serde_json::Map<String, Value> = core
        .vars
        .names
        .iter()
        .zip(&core.vars.values)
        .map(|(name, value)| (name.clone(), var_json(value)))
        .collect();
    let inputs: Vec<Value> = core
        .inputs
        .defs
        .iter()
        .zip(&core.inputs.states)
        .map(|(def, state)| {
            json!({
                "name": def.name,
                "raw": state.raw,
                "yaw": state.yaw,
                "pitch": state.pitch,
            })
        })
        .collect();
    let events: Vec<Value> = core
        .event_log
        .iter()
        .map(|e| json!({ "id": e.id.0, "rig": e.rig, "kind": e.kind.name() }))
        .collect();
    let overrides: serde_json::Map<String, Value> = core
        .overrides()
        .iter()
        .map(|(k, v)| (k.clone(), param_json(v)))
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "asset": asset_name,
        "enabled": enabled,
        "bound": true,
        "target": target,
        "primary": core.primary_name(),
        "time": core.time(),
        "pose": core.pose().as_ref().map(pose_json),
        "rigs": rigs,
        "transition": transition,
        "layers": layers,
        "variables": variables,
        "param_overrides": overrides,
        "inputs": inputs,
        "events": events,
        "skipped": core.rig_infos().iter().map(|r| r.skipped).sum::<u32>(),
    }))
}

/// `camera.set_var { entity, name, value }`.
pub fn camera_set_var(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let name = str_param(params, "name")?;
    let raw = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    let Some(current) = core.vars.get(name) else {
        return Err(framework_error(CameraError::UnknownName(name.to_owned())));
    };
    let value = var_from_json(&current, raw).map_err(framework_error)?;
    core.set_var(name, value).map_err(framework_error)?;
    Ok(json!({ "ok": true }))
}

/// `camera.set_param { entity, name, value }`.
pub fn camera_set_param(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let name = str_param(params, "name")?;
    let raw = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    let Some(current) = core.param(name).cloned() else {
        return Err(framework_error(CameraError::UnknownName(name.to_owned())));
    };
    let value = param_from_json(&current, raw).map_err(framework_error)?;
    core.set_param(name, value).map_err(framework_error)?;
    Ok(json!({ "ok": true }))
}

/// `camera.set_input { entity, slot, value }`: a number (`Axis1`) or `[x, y]`.
pub fn camera_set_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let slot = str_param(params, "slot")?;
    let raw = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let value = if let Some(n) = raw.as_f64() {
        [n as f32, 0.0]
    } else {
        numbers::<2>(raw).ok_or_else(|| {
            framework_error(CameraError::BadValue(
                "`value` must be a number or [x, y] with finite numbers".into(),
            ))
        })?
    };
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    core.set_input(slot, value).map_err(framework_error)?;
    Ok(json!({ "ok": true }))
}

/// `camera.reset_input { entity, slot }`: clears a slot's held raw value and its accumulated
/// angles. A raw value written by `camera.set_input` is held (like a stick position) until the
/// next write; this releases it (zai L2, ADR-1809).
pub fn camera_reset_input(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let slot = str_param(params, "slot")?;
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    core.reset_input(slot).map_err(framework_error)?;
    Ok(json!({ "ok": true }))
}

/// A transition from `{ blend: cut | linear | smooth | ease_in | ease_out, seconds }`.
fn transition_from(value: &Value) -> Result<TransitionDef, RpcError> {
    let kind = crate::params::opt_str(value, "blend")?.unwrap_or("smooth");
    let seconds = match value.get("seconds") {
        None | Some(Value::Null) => 0.25,
        Some(v) => v.as_f64().filter(|s| s.is_finite() && *s >= 0.0).ok_or_else(|| {
            framework_error(CameraError::BadValue(
                "`transition.seconds` must be finite and not negative".into(),
            ))
        })? as f32,
    };
    let blend = match kind {
        "cut" => Blend::Cut,
        "linear" => Blend::Linear(seconds),
        "smooth" => Blend::Smooth(seconds),
        "ease_in" => Blend::EaseIn(seconds),
        "ease_out" => Blend::EaseOut(seconds),
        other => {
            return Err(framework_error(CameraError::BadValue(format!(
                "unknown blend {other:?} (cut | linear | smooth | ease_in | ease_out)"
            ))));
        }
    };
    Ok(TransitionDef {
        from: RigMatch::Any,
        to: RigMatch::Any,
        blend,
        start_facing: StartFacing::OwnSeed,
        hold_outgoing: false,
        share_state: false,
        interruptible: true,
        conditions: vec![],
    })
}

/// `camera.push_overlay { entity, rig, transition?, always_new? }` → `{ id }`.
pub fn camera_push_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let rig = str_param(params, "rig")?;
    let transition = match params.get("transition") {
        None | Some(Value::Null) => None,
        Some(v) => Some(transition_from(v)?),
    };
    let always_new = opt_bool(params, "always_new")?.unwrap_or(false);
    let context = context_param(params)?;
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    let id = core
        .push_overlay(
            rig,
            OverlayOptions {
                transition,
                context,
                always_new,
            },
        )
        .map_err(framework_error)?;
    Ok(json!({ "id": id.0 }))
}

/// `camera.add_additive { entity, rig, order, allow_same_order? }` → `{ id }`.
pub fn camera_add_additive(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let rig = str_param(params, "rig")?;
    let order = crate::params::opt_i64(params, "order")?
        .and_then(|o| i32::try_from(o).ok())
        .ok_or_else(|| RpcError::invalid_params("`order` (an integer) is required"))?;
    let allow = opt_bool(params, "allow_same_order")?.unwrap_or(false);
    let context = context_param(params)?;
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    let id = core
        .add_additive(rig, order, context, allow)
        .map_err(framework_error)?;
    Ok(json!({ "id": id.0 }))
}

/// `camera.release_layer { entity, id?, rig?, instant? }` → `{ released }`.
pub fn camera_release_layer(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let instant = opt_bool(params, "instant")?.unwrap_or(false);
    let by = match (params.get("id"), params.get("rig")) {
        (Some(id), _) if !id.is_null() => {
            let id = id
                .as_u64()
                .and_then(|i| u32::try_from(i).ok())
                .ok_or_else(|| RpcError::invalid_params("`id` must be a layer id"))?;
            ReleaseBy::Id(LayerId(id))
        }
        (_, Some(rig)) if !rig.is_null() => ReleaseBy::Rig(
            rig.as_str()
                .ok_or_else(|| RpcError::invalid_params("`rig` must be a string"))?
                .to_owned(),
            context_param(params)?,
        ),
        _ => return Err(RpcError::invalid_params("give `id` or `rig`")),
    };
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    let released = core.release(by, instant).map_err(framework_error)?;
    Ok(json!({ "released": released }))
}

/// `camera.hold_overlay { entity, id }`.
pub fn camera_hold_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let id = crate::params::opt_u64(params, "id")?
        .and_then(|i| u32::try_from(i).ok())
        .ok_or_else(|| RpcError::invalid_params("`id` (a layer id) is required"))?;
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    core.hold_overlay(LayerId(id)).map_err(framework_error)?;
    Ok(json!({ "ok": true }))
}

fn driver_summary(world: &World, entity: Entity) -> Value {
    let Some(driver) = world.get::<CameraDriver>(entity) else {
        return Value::Null;
    };
    json!({
        "entity": entity.to_bits(),
        "asset": driver.asset,
        "target": driver.target.map(|t| t.to_bits()),
        "enabled": driver.enabled,
        "local_player": driver.local_player,
        "bound": driver.core().is_some(),
        "status": world.resource::<CameraAssets>().status(&driver.asset).map(CameraBuildStatus::name),
    })
}

/// `camera.set_driver { entity, asset?, target?, enabled? }`: attaches or changes the driver on a
/// camera entity.
pub fn camera_set_driver(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "scene.query_entities lists live entities",
        ));
    }
    let asset = match params.get("asset") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params("`asset` must be a string"))?
                .to_owned(),
        ),
    };
    let target = match params.get("target") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let bits = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`target` must be an entity id or null"))?;
            Some(Some(Entity::try_from_bits(bits).ok_or_else(|| {
                RpcError::invalid_params(format!("{bits} is not an entity id"))
            })?))
        }
    };
    let enabled = opt_bool(params, "enabled")?;
    if let Some(name) = &asset {
        let assets = world.resource::<CameraAssets>();
        if assets.working(name).is_none() {
            return Err(missing(
                "camera_unknown_asset",
                format!("no camera asset named {name:?}"),
                "camera.assets lists the loaded assets",
            ));
        }
        if assets.active(name).is_none() {
            let issues = assets.issues(name).unwrap_or(&[]);
            let unknown_hook = issues.iter().any(|i| i.code == "camera_unknown_hook");
            let mut error = invalid(
                if unknown_hook {
                    "camera_unknown_hook"
                } else {
                    "camera_asset_invalid"
                },
                format!("camera asset {name:?} has errors and no earlier good version"),
                "fix the issues in data.issues (camera.validate lists them) or register the director hook",
            );
            if let Some(Value::Object(data)) = error.data.as_mut() {
                data.insert(
                    "issues".into(),
                    json!(issues.iter().map(issue_json).collect::<Vec<_>>()),
                );
            }
            return Err(error);
        }
    }
    if world.get::<CameraDriver>(entity).is_none() {
        let Some(name) = &asset else {
            return Err(missing(
                "camera_no_driver",
                format!(
                    "entity {} has no CameraDriver and no `asset` was given",
                    entity.to_bits()
                ),
                "pass `asset` to attach a driver",
            ));
        };
        world.entity_mut(entity).insert(CameraDriver::new(name.clone()));
    }
    if let Some(mut driver) = world.get_mut::<CameraDriver>(entity) {
        if let Some(name) = asset
            && driver.asset != name
        {
            driver.asset = name;
            // Rebinding happens in the camera system on the next frame.
            driver.unbind();
        }
        if let Some(target) = target {
            driver.target = target;
        }
        if let Some(enabled) = enabled {
            driver.enabled = enabled;
        }
    }
    Ok(driver_summary(world, entity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::camera::CameraPlugin;
    use regia_framework::camera::asset::CameraAsset;
    use regia_scene::Transform;
    use std::time::Duration;

    const ASSET: &str = r#"(format_version: 1, name: "t",
        params: {"len": Float(5.0), "mode": Name("a"), "pos": Vec3((0.0, 1.0, 2.0)), "count": Int(2), "on": Bool(true)},
        variables: [
            (name: "aim", default: Bool(false)),
            (name: "speed", default: Float(0.0)),
            (name: "dir", default: Vec3((0.0, 0.0, 1.0))),
            (name: "face", default: YawPitch(0.0, 0.0)),
        ],
        inputs: [(name: "look", kind: Axis2, speed: (90.0, 90.0))],
        rigs: [
            (name: "main", root: [Boom(length: Param("len")), Fov(degrees: Literal(60.0)), Group(enabled: Param("on"), nodes: [])]),
            (name: "aim", root: [Fov(degrees: Literal(30.0))]),
            (name: "bump", root: [Offset(translation: (0.0, 1.0, 0.0), space: World)]),
        ],
        director: Fixed("main"))"#;

    fn app_with_driver() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let asset = CameraAsset::from_ron(ASSET).expect("asset parses");
        app.world_mut().resource_mut::<CameraAssets>().insert(
            asset,
            Some("t.camera.ron".into()),
            &CameraHooks::default(),
        );
        let camera = app
            .world_mut()
            .spawn((Transform::default(), CameraDriver::new("t")))
            .id();
        app.update();
        app.update();
        (app, camera)
    }

    fn reason(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    #[test]
    fn without_the_camera_plugin_every_method_is_unsupported() {
        let mut empty = App::new();
        let w = empty.world_mut();
        for error in [
            camera_assets(w, &json!({})).expect_err("a"),
            camera_validate(w, &json!({ "asset": "x" })).expect_err("v"),
            camera_inspect(w, &json!({ "entity": 1 })).expect_err("i"),
            camera_set_var(w, &json!({ "entity": 1, "name": "x", "value": 1 })).expect_err("sv"),
            camera_set_driver(w, &json!({ "entity": 1 })).expect_err("sd"),
        ] {
            assert_eq!(error.code, protocol::UNSUPPORTED);
        }
    }

    #[test]
    fn assets_validate_and_inspect_round_trip() {
        let (mut app, camera) = app_with_driver();
        let w = app.world_mut();
        let assets = camera_assets(w, &json!({})).expect("assets");
        assert_eq!(assets["count"], 1);
        assert_eq!(assets["assets"][0]["name"], "t");
        assert_eq!(
            assets["assets"][0]["status"], "warnings",
            "unused params are warnings"
        );
        assert_eq!(assets["assets"][0]["rigs"], json!(["main", "aim", "bump"]));
        let valid = camera_validate(w, &json!({ "asset": "t" })).expect("validate");
        assert!(
            valid["issues"]
                .as_array()
                .expect("issues")
                .iter()
                .any(|i| i["code"] == "camera_unused_param")
        );
        let inspected = camera_inspect(w, &json!({ "entity": camera.to_bits() })).expect("inspect");
        assert_eq!(inspected["bound"], true);
        assert_eq!(inspected["primary"], "main");
        assert_eq!(inspected["rigs"][0]["name"], "main");
        let trace = inspected["rigs"][0]["trace"].as_array().expect("trace");
        assert_eq!(trace[0]["node"], "boom");
        assert_eq!(trace[1]["node"], "fov");
        assert!(inspected["pose"]["fov_deg"].as_f64().is_some());
        // Reproducible: a second read of the same frame gives the same trace.
        let again = camera_inspect(w, &json!({ "entity": camera.to_bits() })).expect("inspect again");
        assert_eq!(inspected["rigs"][0]["trace"], again["rigs"][0]["trace"]);
    }

    #[test]
    fn variables_parameters_and_inputs_round_trip() {
        let (mut app, camera) = app_with_driver();
        let e = camera.to_bits();
        let w = app.world_mut();
        for (name, value) in [
            ("aim", json!(true)),
            ("speed", json!(3.5)),
            ("dir", json!([1.0, 0.0, 0.0])),
            ("face", json!({ "yaw": 0.5, "pitch": 0.1 })),
        ] {
            camera_set_var(w, &json!({ "entity": e, "name": name, "value": value })).expect(name);
        }
        for (name, value) in [
            ("len", json!(8.0)),
            ("mode", json!("b")),
            ("pos", json!([1.0, 2.0, 3.0])),
            ("count", json!(7)),
            ("on", json!(false)),
        ] {
            camera_set_param(w, &json!({ "entity": e, "name": name, "value": value })).expect(name);
        }
        camera_set_input(w, &json!({ "entity": e, "slot": "look", "value": [1.0, 0.5] })).expect("input");
        camera_set_input(w, &json!({ "entity": e, "slot": "look", "value": 0.25 })).expect("axis1 form");
        let seen = camera_inspect(w, &json!({ "entity": e })).expect("inspect");
        assert_eq!(seen["variables"]["aim"], json!(true));
        assert_eq!(seen["variables"]["dir"], json!([1.0, 0.0, 0.0]));
        assert_eq!(seen["param_overrides"]["len"], json!(8.0));
        assert_eq!(seen["param_overrides"]["mode"], json!("b"));
        assert_eq!(seen["inputs"][0]["raw"], json!([0.25, 0.0]));
        camera_reset_input(w, &json!({ "entity": e, "slot": "look" })).expect("reset");
        let seen = camera_inspect(w, &json!({ "entity": e })).expect("inspect");
        assert_eq!(
            seen["inputs"][0]["raw"],
            json!([0.0, 0.0]),
            "the held value is released"
        );
        let err = camera_reset_input(w, &json!({ "entity": e, "slot": "nope" })).expect_err("unknown");
        assert_eq!(
            err.data.as_ref().and_then(|d| d["reason"].as_str()),
            Some("camera_unknown_name")
        );
    }

    #[test]
    fn layers_round_trip_and_events_are_listed() {
        let (mut app, camera) = app_with_driver();
        let e = camera.to_bits();
        let w = app.world_mut();
        let overlay = camera_push_overlay(
            w,
            &json!({ "entity": e, "rig": "aim", "transition": { "blend": "linear", "seconds": 0.5 } }),
        )
        .expect("overlay");
        let again = camera_push_overlay(w, &json!({ "entity": e, "rig": "aim" })).expect("reuse");
        assert_eq!(overlay["id"], again["id"], "same rig reuses the layer");
        let fresh =
            camera_push_overlay(w, &json!({ "entity": e, "rig": "aim", "always_new": true })).expect("new");
        assert_ne!(overlay["id"], fresh["id"]);
        let additive =
            camera_add_additive(w, &json!({ "entity": e, "rig": "bump", "order": 3 })).expect("additive");
        assert!(camera_hold_overlay(w, &json!({ "entity": e, "id": fresh["id"] })).is_ok());
        let released =
            camera_release_layer(w, &json!({ "entity": e, "id": additive["id"], "instant": true }))
                .expect("release");
        assert_eq!(released["released"], 1);
        let by_rig =
            camera_release_layer(w, &json!({ "entity": e, "rig": "aim", "instant": true })).expect("by rig");
        assert_eq!(by_rig["released"], 2);
        let seen = camera_inspect(w, &json!({ "entity": e })).expect("inspect");
        assert!(!seen["events"].as_array().expect("events").is_empty());
    }

    #[test]
    fn context_is_passed_to_layers_and_a_bad_one_is_refused() {
        let (mut app, camera) = app_with_driver();
        let e = camera.to_bits();
        let w = app.world_mut();
        let none = camera_push_overlay(w, &json!({ "entity": e, "rig": "aim" })).expect("plain");
        let a =
            camera_push_overlay(w, &json!({ "entity": e, "rig": "aim", "context": "boss" })).expect("named");
        let a2 = camera_push_overlay(w, &json!({ "entity": e, "rig": "aim", "context": "boss" }))
            .expect("named again");
        let point = camera_push_overlay(
            w,
            &json!({ "entity": e, "rig": "aim", "context": [1.0, 2.0, 3.0] }),
        )
        .expect("point");
        assert_ne!(none["id"], a["id"], "a context makes a separate layer");
        assert_eq!(a["id"], a2["id"], "the same context reuses the layer");
        assert_ne!(a["id"], point["id"]);
        let extra = camera_add_additive(
            w,
            &json!({ "entity": e, "rig": "bump", "order": 1, "context": "boss" }),
        )
        .expect("additive with context");
        assert!(extra["id"].is_u64());
        let released = camera_release_layer(
            w,
            &json!({ "entity": e, "rig": "aim", "context": "boss", "instant": true }),
        )
        .expect("release by rig and context");
        assert_eq!(released["released"], 1, "only the layer with that context");
        for bad in [
            json!(5),
            json!([1.0, 2.0]),
            json!({ "x": 1 }),
            // A huge finite f64 casts to an infinite f32: refused, not stored as a target.
            json!([1e300, 0.0, 0.0]),
            json!([f64::INFINITY, 0.0, 0.0]),
            json!([f64::NAN, 0.0, 0.0]),
        ] {
            let err = camera_push_overlay(w, &json!({ "entity": e, "rig": "aim", "context": bad }))
                .expect_err("bad context");
            assert_eq!(err.code, protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn set_driver_attaches_changes_and_reports() {
        let (mut app, camera) = app_with_driver();
        let bare = app.world_mut().spawn(Transform::default()).id();
        let target = app.world_mut().spawn(Transform::default()).id();
        let w = app.world_mut();
        let made = camera_set_driver(
            w,
            &json!({ "entity": bare.to_bits(), "asset": "t", "target": target.to_bits() }),
        )
        .expect("attach");
        assert_eq!(made["asset"], "t");
        assert_eq!(made["target"], target.to_bits());
        let off =
            camera_set_driver(w, &json!({ "entity": camera.to_bits(), "enabled": false })).expect("disable");
        assert_eq!(off["enabled"], false);
        let cleared =
            camera_set_driver(w, &json!({ "entity": bare.to_bits(), "target": null })).expect("clear target");
        assert_eq!(cleared["target"], Value::Null);
    }

    #[test]
    fn load_reads_a_project_file_and_assets_lists_it() {
        let dir = regia_test_utils::unique_temp_dir("camera-agent-load");
        let file = dir.join("assets/cam/t.camera.ron");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("dirs");
        regia_test_utils::write_atomic(&file, ASSET.as_bytes()).expect("write");
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin));
        app.insert_resource(
            regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
                project_root: dir.clone(),
                platform: regia_asset::current_platform(),
                content_dir: None,
            })
            .expect("open"),
        );
        let world = app.world_mut();
        let answer = camera_load(world, &json!({ "path": "assets/cam/t.camera.ron" })).expect("loads");
        assert_eq!(answer["path"], "cam/t.camera.ron");
        assert_eq!(answer["asset"], "t");
        assert_eq!(answer["status"], "warnings", "the asset has an unused parameter");
        assert_eq!(answer["cooked"], false);
        assert_eq!(answer["generation"], 1);
        assert!(!answer["issues"].as_array().expect("issues").is_empty());
        let listed = camera_assets(world, &json!({})).expect("assets");
        assert_eq!(listed["assets"][0]["path"], "cam/t.camera.ron");
        assert_eq!(listed["files"][0]["path"], "cam/t.camera.ron");
        assert_eq!(listed["files"][0]["asset"], "t");
        assert_eq!(listed["files"][0]["error"], Value::Null);

        let code = |e: RpcError| e.data.and_then(|d| d["reason"].as_str().map(str::to_owned));
        let e = camera_load(world, &json!({ "path": "../t.camera.ron" })).expect_err("path");
        assert_eq!(code(e).as_deref(), Some("camera_bad_path"));
        let e = camera_load(world, &json!({ "path": "none.camera.ron" })).expect_err("missing");
        assert_eq!(e.code, protocol::NOT_FOUND);
        assert_eq!(code(e).as_deref(), Some("camera_file_not_found"));
        regia_test_utils::write_atomic(&dir.join("assets/b.camera.ron"), b"(").expect("write");
        let e = camera_load(world, &json!({ "path": "b.camera.ron" })).expect_err("broken");
        assert_eq!(code(e).as_deref(), Some("camera_asset_invalid"));
        let listed = camera_assets(world, &json!({})).expect("assets");
        let files = listed["files"].as_array().expect("files");
        assert!(
            files
                .iter()
                .any(|f| f["path"] == "b.camera.ron" && f["error"]["reason"] == "camera_asset_invalid")
        );
        let e = camera_load(world, &json!({})).expect_err("no path");
        assert_eq!(e.code, protocol::INVALID_PARAMS);
        let mut bare = World::new();
        let e = camera_load(&mut bare, &json!({ "path": "a.camera.ron" })).expect_err("no framework");
        assert_eq!(e.code, protocol::UNSUPPORTED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_error_code_is_produced() {
        let (mut app, camera) = app_with_driver();
        let bare = app.world_mut().spawn(Transform::default()).id();
        let e = camera.to_bits();
        // An asset that fails validation without an earlier good version.
        let mut broken = CameraAsset::from_ron(ASSET).expect("parses");
        broken.name = "broken".into();
        broken.director = regia_framework::camera::asset::DirectorDef::Fixed("ghost".into());
        let mut hooked = broken.clone();
        hooked.name = "hooked".into();
        hooked.director = regia_framework::camera::asset::DirectorDef::Hook("missing_hook".into());
        {
            let mut assets = app.world_mut().resource_mut::<CameraAssets>();
            assets.insert(broken, None, &CameraHooks::default());
            assets.insert(hooked, None, &CameraHooks::default());
        }
        let w = app.world_mut();
        let cases: Vec<(&str, RpcError)> = vec![
            (
                "camera_no_driver",
                camera_set_var(
                    w,
                    &json!({ "entity": bare.to_bits(), "name": "aim", "value": true }),
                )
                .expect_err("nodriver"),
            ),
            (
                "camera_unknown_asset",
                camera_validate(w, &json!({ "asset": "nope" })).expect_err("asset"),
            ),
            (
                "camera_unknown_rig",
                camera_push_overlay(w, &json!({ "entity": e, "rig": "nope" })).expect_err("rig"),
            ),
            (
                "camera_unknown_hook",
                camera_set_driver(w, &json!({ "entity": e, "asset": "hooked" })).expect_err("hook"),
            ),
            (
                "camera_unknown_name",
                camera_set_var(w, &json!({ "entity": e, "name": "ghost", "value": 1 })).expect_err("name"),
            ),
            (
                "camera_bad_value",
                camera_set_var(w, &json!({ "entity": e, "name": "aim", "value": 3.0 })).expect_err("value"),
            ),
            (
                "camera_unknown_layer_id",
                camera_hold_overlay(w, &json!({ "entity": e, "id": 999 })).expect_err("layer"),
            ),
            ("camera_order_taken", {
                camera_add_additive(w, &json!({ "entity": e, "rig": "bump", "order": 1 })).expect("first");
                camera_add_additive(w, &json!({ "entity": e, "rig": "bump", "order": 1 })).expect_err("order")
            }),
            (
                "camera_asset_invalid",
                camera_set_driver(w, &json!({ "entity": e, "asset": "broken" })).expect_err("invalid"),
            ),
        ];
        for (code, error) in cases {
            assert_eq!(reason(&error).as_deref(), Some(code), "{code}: {error:?}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
        }
        // The invalid asset's issues travel in the error data.
        let invalid = camera_set_driver(w, &json!({ "entity": e, "asset": "broken" })).expect_err("invalid");
        assert!(
            invalid.data.as_ref().expect("data")["issues"]
                .as_array()
                .is_some_and(|i| !i.is_empty())
        );
    }
}
