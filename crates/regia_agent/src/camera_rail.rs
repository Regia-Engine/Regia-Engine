// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the rail camera and the crane (ADR-3231, book/agents/camera-rail-crane.md):
//! `camera.rail_state` and `camera.crane_state` read what the `Rail` and `Crane` nodes did in the
//! last evaluation, `camera.set_rail` and `camera.set_crane` write the driver variables the nodes
//! read (`rail_position`, `crane_yaw`, ...), the same ones a sequence track keys.
//!
//! Handlers are plain functions over `&mut World`. A world without `CameraPlugin` answers
//! `UNSUPPORTED`. Errors carry `data.reason`, one of the lowercase codes on the book page.

use regia_app::prelude::*;
use regia_framework::camera::asset::VarValue;
use regia_framework::camera::crane::AXIS_NAMES;
use regia_framework::camera::driver::CameraDriver;
use regia_framework::camera::rail_keys::KEYED;
use serde_json::{Value, json};

use crate::camera_framework::{
    bound_core, driver_mut, entity_param, framework_error, invalid, require_framework,
};
use crate::protocol::RpcError;

/// Most drivers a state call lists.
const MAX_LISTED: usize = 64;

/// The driver entities, ascending, optionally only the one named by `entity`.
fn drivers(world: &mut World, params: &Value) -> Result<Vec<Entity>, RpcError> {
    if params.get("entity").is_some_and(|v| !v.is_null()) {
        let entity = entity_param(params)?;
        driver_mut(world, entity)?;
        return Ok(vec![entity]);
    }
    let mut query = world.query::<(Entity, &CameraDriver)>();
    let mut all: Vec<Entity> = query.iter(world).map(|(e, _)| e).collect();
    all.sort_by_key(|e| e.to_bits());
    all.truncate(MAX_LISTED);
    Ok(all)
}

fn vec3_json(v: regia_core::math::Vec3) -> Value {
    json!(v.to_array())
}

/// `camera.rail_state { entity? }`: the rail nodes of the active rigs, per camera driver.
/// An explicitly named driver whose asset is not bound answers `bound: false` instead of an
/// empty success, matching `camera.inspect`.
pub fn camera_rail_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let explicit = params.get("entity").is_some_and(|v| !v.is_null());
    let entities = drivers(world, params)?;
    let mut cameras = Vec::new();
    for entity in entities {
        let mut driver = driver_mut(world, entity)?;
        let Some(core) = driver.core_mut() else {
            if explicit {
                cameras.push(json!({ "entity": entity.to_bits(), "bound": false }));
            }
            continue;
        };
        let mut rails = Vec::new();
        for rig in core.rig_infos() {
            for r in &rig.rails {
                rails.push(json!({
                    "rig": rig.name,
                    "node": r.node,
                    "spline": r.spline,
                    "distance_m": r.distance_m,
                    "length_m": r.length_m,
                    "closed": r.closed,
                    "clamped": r.clamped,
                    "at_end": r.at_end,
                    "point": vec3_json(r.point),
                    "tangent": vec3_json(r.tangent),
                    "goal_m": r.goal_m,
                }));
            }
        }
        cameras.push(json!({
            "entity": entity.to_bits(),
            "rail_position": core.vars.get_f32("rail_position"),
            "rails": rails,
        }));
    }
    Ok(json!({ "count": cameras.len(), "cameras": cameras }))
}

/// `camera.crane_state { entity? }`: the crane nodes of the active rigs, per camera driver.
/// An explicitly named driver whose asset is not bound answers `bound: false` instead of an
/// empty success, matching `camera.inspect`.
pub fn camera_crane_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let explicit = params.get("entity").is_some_and(|v| !v.is_null());
    let entities = drivers(world, params)?;
    let mut cameras = Vec::new();
    for entity in entities {
        let mut driver = driver_mut(world, entity)?;
        let Some(core) = driver.core_mut() else {
            if explicit {
                cameras.push(json!({ "entity": entity.to_bits(), "bound": false }));
            }
            continue;
        };
        let mut cranes = Vec::new();
        for rig in core.rig_infos() {
            for c in &rig.cranes {
                let axes = |a: regia_framework::camera::crane::CraneAxes| {
                    let mut map = serde_json::Map::new();
                    for (name, value) in AXIS_NAMES.iter().zip(a.to_array()) {
                        map.insert((*name).to_owned(), json!(value));
                    }
                    Value::Object(map)
                };
                let clamped: Vec<&str> = AXIS_NAMES
                    .iter()
                    .zip(c.clamped)
                    .filter(|(_, moved)| *moved)
                    .map(|(name, _)| *name)
                    .collect();
                cranes.push(json!({
                    "rig": rig.name,
                    "node": c.node,
                    "base": vec3_json(c.base),
                    "heading_deg": c.heading_deg,
                    "requested": axes(c.requested),
                    "axes": axes(c.axes),
                    "clamped": clamped,
                    "tip": vec3_json(c.tip),
                }));
            }
        }
        cameras.push(json!({ "entity": entity.to_bits(), "cranes": cranes }));
    }
    Ok(json!({ "count": cameras.len(), "cameras": cameras }))
}

/// Writes the float variable `var` of a driver, or refuses with `rail_var_not_declared`.
fn write_float(world: &mut World, entity: Entity, var: &str, value: f32) -> Result<(), RpcError> {
    let mut driver = driver_mut(world, entity)?;
    let core = bound_core(&mut driver)?;
    if !matches!(core.vars.get(var), Some(VarValue::Float(_))) {
        return Err(invalid(
            "rail_var_not_declared",
            format!("the camera asset declares no Float variable `{var}`"),
            "declare it under `variables` in the .camera.ron and read it with Var(\"...\") in the node",
        ));
    }
    core.set_var(var, VarValue::Float(value)).map_err(framework_error)
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(Some(n as f32)),
            _ => Err(RpcError::invalid_params(format!(
                "`{field}` must be a finite number"
            ))),
        },
    }
}

/// `camera.set_rail { entity, position }`: sets the `rail_position` variable (meters).
pub fn camera_set_rail(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let position = number(params, "position")?
        .ok_or_else(|| RpcError::invalid_params("`position` (meters) is required"))?;
    write_float(world, entity, "rail_position", position)?;
    Ok(json!({ "ok": true, "rail_position": position }))
}

/// `camera.set_crane { entity, yaw?, pitch?, length?, mount_yaw?, mount_pitch? }`: sets the
/// `crane_*` variables that were given. All are validated before any is written.
pub fn camera_set_crane(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_framework(world)?;
    let entity = entity_param(params)?;
    let mut writes = Vec::new();
    for (path, var) in KEYED.iter().filter(|(p, _)| p.starts_with("camera.crane.")) {
        let field = path.trim_start_matches("camera.crane.");
        if let Some(value) = number(params, field)? {
            writes.push((*var, value));
        }
    }
    if writes.is_empty() {
        return Err(RpcError::invalid_params(
            "give at least one of yaw, pitch, length, mount_yaw, mount_pitch",
        ));
    }
    {
        let mut driver = driver_mut(world, entity)?;
        let core = bound_core(&mut driver)?;
        for (var, _) in &writes {
            if !matches!(core.vars.get(var), Some(VarValue::Float(_))) {
                return Err(invalid(
                    "rail_var_not_declared",
                    format!("the camera asset declares no Float variable `{var}`"),
                    "declare it under `variables` in the .camera.ron and read it with Var(\"...\") in the node",
                ));
            }
        }
    }
    let mut written = serde_json::Map::new();
    for (var, value) in writes {
        write_float(world, entity, var, value)?;
        written.insert(var.to_owned(), json!(value));
    }
    Ok(json!({ "ok": true, "written": Value::Object(written) }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_core::math::Vec3;
    use regia_framework::camera::CameraPlugin;
    use regia_framework::camera::asset::CameraAsset;
    use regia_framework::camera::director::CameraHooks;
    use regia_framework::camera::driver::CameraAssets;
    use regia_scene::components::Name;
    use regia_scene::spline::Spline;
    use regia_scene::{GlobalTransform, Transform};

    use super::*;

    const ASSET: &str = r#"(format_version: 2, name: "rc",
        variables: [(name: "rail_position", default: Float(0.0)), (name: "crane_yaw", default: Float(0.0)),
                    (name: "flag", default: Bool(false))],
        rigs: [
          (name: "dolly", root: [Rail(spline: "track", drive: Value(Var("rail_position")))]),
          (name: "arm", root: [Crane(base: Ground((0.0, 1.0, 0.0)), yaw: Var("crane_yaw"), length: Literal(3.0),
                                     limits: (yaw_deg: Some((-30.0, 30.0))))]),
        ],
        director: Fixed("dolly"))"#;

    fn app_with(director: &str) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let mut asset = CameraAsset::from_ron(ASSET).expect("asset parses");
        asset.director = regia_framework::camera::asset::DirectorDef::Fixed(director.into());
        app.world_mut()
            .resource_mut::<CameraAssets>()
            .insert(asset, None, &CameraHooks::default());
        app.world_mut().spawn((
            Name::new("track"),
            Spline::through([Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0)]),
            Transform::default(),
            GlobalTransform::IDENTITY,
        ));
        let camera = app
            .world_mut()
            .spawn((Transform::default(), CameraDriver::new("rc")))
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
            .map(str::to_owned)
    }

    #[test]
    fn without_the_camera_plugin_every_method_is_unsupported() {
        let mut empty = App::new();
        let w = empty.world_mut();
        for result in [
            camera_rail_state(w, &json!({})),
            camera_crane_state(w, &json!({})),
            camera_set_rail(w, &json!({ "entity": 1, "position": 1.0 })),
            camera_set_crane(w, &json!({ "entity": 1, "yaw": 1.0 })),
        ] {
            assert_eq!(
                result.expect_err("unsupported").code,
                crate::protocol::UNSUPPORTED
            );
        }
    }

    #[test]
    fn set_rail_moves_the_camera_and_rail_state_reports_it() {
        let (mut app, camera) = app_with("dolly");
        let e = camera.to_bits();
        let set = camera_set_rail(app.world_mut(), &json!({ "entity": e, "position": 12.5 })).expect("set");
        assert_eq!(set["rail_position"], 12.5);
        app.update();
        let state = camera_rail_state(app.world_mut(), &json!({ "entity": e })).expect("state");
        let rail = &state["cameras"][0]["rails"][0];
        assert_eq!(rail["spline"], "track");
        assert!((rail["distance_m"].as_f64().expect("distance") - 12.5).abs() < 1.0e-4);
        assert_eq!(
            rail["length_m"].as_f64().map(|l| (l - 20.0).abs() < 1.0e-3),
            Some(true)
        );
        assert_eq!(rail["closed"], false);
        let x = app.world().get::<Transform>(camera).expect("t").translation.x;
        assert!((x - 12.5).abs() < 1.0e-3, "{x}");
        // Past the end: clamped and reported.
        camera_set_rail(app.world_mut(), &json!({ "entity": e, "position": 99.0 })).expect("set");
        app.update();
        let state = camera_rail_state(app.world_mut(), &json!({})).expect("all drivers");
        assert_eq!(state["count"], 1);
        assert_eq!(state["cameras"][0]["rails"][0]["clamped"], true);
        assert_eq!(state["cameras"][0]["rails"][0]["at_end"], true);
    }

    #[test]
    fn set_crane_clamps_in_the_node_and_crane_state_shows_both_values() {
        let (mut app, camera) = app_with("arm");
        let e = camera.to_bits();
        let set = camera_set_crane(app.world_mut(), &json!({ "entity": e, "yaw": 80.0 })).expect("set");
        assert_eq!(set["written"]["crane_yaw"], 80.0);
        app.update();
        let state = camera_crane_state(app.world_mut(), &json!({ "entity": e })).expect("state");
        let crane = &state["cameras"][0]["cranes"][0];
        assert_eq!(crane["requested"]["yaw"], 80.0);
        assert_eq!(crane["axes"]["yaw"], 30.0);
        assert_eq!(crane["clamped"], json!(["yaw"]));
    }

    #[test]
    fn errors_carry_reasons() {
        let (mut app, camera) = app_with("dolly");
        let e = camera.to_bits();
        let w = app.world_mut();
        // The asset declares no crane_length: nothing is written.
        let err =
            camera_set_crane(w, &json!({ "entity": e, "yaw": 5.0, "length": 2.0 })).expect_err("undeclared");
        assert_eq!(reason(&err).as_deref(), Some("rail_var_not_declared"));
        assert_eq!(
            app.world()
                .get::<CameraDriver>(camera)
                .and_then(|d| d.core())
                .and_then(|c| c.vars.get_f32("crane_yaw")),
            Some(0.0),
            "refused calls write nothing"
        );
        let w = app.world_mut();
        let plain = w.spawn(Transform::default()).id();
        let err = camera_set_rail(w, &json!({ "entity": plain.to_bits(), "position": 1.0 }))
            .expect_err("no driver");
        assert_eq!(reason(&err).as_deref(), Some("camera_no_driver"));
        assert!(
            camera_set_rail(w, &json!({ "entity": e })).is_err(),
            "position is required"
        );
        assert!(camera_set_rail(w, &json!({ "entity": e, "position": "far" })).is_err());
        assert!(
            camera_set_crane(w, &json!({ "entity": e })).is_err(),
            "at least one axis"
        );
        assert!(camera_rail_state(w, &json!({ "entity": 99_999_999_999_u64 })).is_err());
    }

    #[test]
    fn an_explicit_unbound_driver_reports_bound_false() {
        // A driver whose asset failed to load has no bound core: naming it explicitly answers
        // `bound: false` (like camera.inspect), not an empty success; a list call skips it.
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin));
        let camera = app
            .world_mut()
            .spawn((Transform::default(), CameraDriver::new("missing")))
            .id();
        app.update();
        let e = camera.to_bits();
        let state = camera_rail_state(app.world_mut(), &json!({ "entity": e })).expect("state");
        assert_eq!(state["count"], 1);
        assert_eq!(state["cameras"][0]["bound"], false);
        assert!(
            !state["cameras"][0]
                .as_object()
                .expect("object")
                .contains_key("rails")
        );
        let state = camera_crane_state(app.world_mut(), &json!({ "entity": e })).expect("state");
        assert_eq!(state["count"], 1);
        assert_eq!(state["cameras"][0]["bound"], false);
        let state = camera_rail_state(app.world_mut(), &json!({})).expect("all drivers");
        assert_eq!(state["count"], 0, "unbound drivers are skipped in a list call");
    }
}
