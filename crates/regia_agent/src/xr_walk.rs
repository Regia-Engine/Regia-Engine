// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the XR walk mode (ADR-2457, book `agents/xr-walk.md`):
//! `xr.walk_start` starts a seated or standing walk-through (on the simulator it also takes the
//! session to focused), `xr.walk_set` changes locomotion, comfort and the height calibration, and
//! `xr.get` gains a `walk` object.

use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `refused` code when the XR plugin is not installed.
const NOT_INSTALLED: &str = "xr_not_installed";
/// `refused` code when XR is installed but the walk plugin is not.
#[cfg(feature = "xr")]
const WALK_NOT_INSTALLED: &str = "xr_walk_not_installed";

/// `xr.walk_start { position?, yaw?, preset?, budget?, focus? }`.
pub(crate) fn xr_walk_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::walk_start(world, params);
    #[cfg(not(feature = "xr"))]
    {
        let _ = world;
        // Nothing reads the parameters here; a wrong-typed one is still refused (ADR-2740).
        crate::params::check_types("xr.walk_start", params)?;
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

/// `xr.walk_set { active?, locomotion?, move_speed_mps?, teleport_range_m?, ground_follow?, preset?,
/// calibrate_height? }`.
pub(crate) fn xr_walk_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::walk_set(world, params);
    #[cfg(not(feature = "xr"))]
    {
        let _ = world;
        // Nothing reads the parameters here; a wrong-typed one is still refused (ADR-2740).
        crate::params::check_types("xr.walk_set", params)?;
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

/// Adds `walk` to an `xr.get` answer when the walk plugin is installed.
pub(crate) fn merge_walk(world: &mut World, out: Value) -> Value {
    #[cfg(feature = "xr")]
    return ext::merge_walk(world, out);
    #[cfg(not(feature = "xr"))]
    {
        let _ = world;
        out
    }
}

#[cfg(feature = "xr")]
mod ext {
    use super::*;
    use regia_core::glam::Vec3;
    use regia_platform::xr::{Pose, XrError};
    use regia_scene::Transform;
    use regia_xr::sim::ScriptState;
    use regia_xr::{ComfortPreset, Locomotion, WalkStart, XrAppState, XrGround, XrSession, XrState, XrWalk};

    fn xr_error(err: &XrError) -> RpcError {
        let mut e = RpcError::new(
            crate::protocol::INVALID_PARAMS,
            "F0002",
            err.to_string(),
            "xr.get shows the walk (walk) and the session (app_state); see book/agents/xr-walk.md",
        );
        if let Some(Value::Object(data)) = e.data.as_mut() {
            data.insert("reason".into(), json!(err.code()));
        }
        e
    }

    fn invalid(key: &str, reason: &str) -> RpcError {
        xr_error(&XrError::InvalidValue {
            key: key.to_string(),
            reason: reason.to_string(),
        })
    }

    fn vec3(v: Vec3) -> Value {
        json!([v.x, v.y, v.z])
    }

    /// `None` when the method can run, else the `refused` answer.
    fn refused(world: &World) -> Option<Value> {
        if !world.contains_resource::<XrSession>() {
            return Some(json!({ "refused": NOT_INSTALLED, "installed": false }));
        }
        if !world.contains_resource::<XrWalk>() {
            return Some(json!({ "refused": WALK_NOT_INSTALLED, "installed": true }));
        }
        None
    }

    fn finite(v: &Value) -> Option<f32> {
        v.as_f64().map(|v| v as f32).filter(|v| v.is_finite())
    }

    /// `[x, y, z]` or `[x, z]` (height from the ground).
    fn position(v: &Value) -> Result<Vec3, RpcError> {
        let bad = || invalid("position", "[x, y, z] or [x, z], finite numbers");
        let a = v.as_array().ok_or_else(bad)?;
        let n: Vec<f32> = a.iter().map(finite).collect::<Option<Vec<_>>>().ok_or_else(bad)?;
        match n.as_slice() {
            [x, y, z] => Ok(Vec3::new(*x, *y, *z)),
            [x, z] => Ok(Vec3::new(*x, 0.0, *z)),
            _ => Err(bad()),
        }
    }

    fn preset(v: &Value) -> Result<ComfortPreset, RpcError> {
        v.as_str()
            .and_then(ComfortPreset::from_name)
            .ok_or_else(|| invalid("preset", "\"comfortable\", \"moderate\" or \"free\""))
    }

    fn head_world(world: &mut World) -> Option<Vec3> {
        let (offset, held) = {
            let s = world.get_resource::<XrSession>()?;
            (s.offset, s.held_head())
        };
        let mut rigs = world.query_filtered::<&Transform, regia_xr::plugin::RigFilter>();
        let rig = rigs.single(world).ok()?;
        let rig = Pose {
            position: rig.translation,
            orientation: rig.rotation,
        };
        Some(rig.compose(&offset.compose(&held)).position)
    }

    fn walk_json(world: &mut World) -> Value {
        let head = head_world(world);
        let has_ground = world.contains_resource::<XrGround>();
        let app_state = world.get_resource::<XrState>().map(|s| s.app.name());
        let Some(w) = world.get_resource::<XrWalk>() else {
            return Value::Null;
        };
        let budget = w.budget.map_or(Value::Null, |b| {
            json!({
                "refresh_hz": b.refresh_hz,
                "frame_ms": b.frame_ms,
                "render_scale": b.render_scale,
                "foveation": b.foveation.name(),
            })
        });
        json!({
            "active": w.active,
            "locomotion": w.settings.locomotion.name(),
            "move_speed_mps": w.settings.move_speed_mps,
            "teleport_range_m": w.settings.teleport_range_m,
            "ground_follow": w.settings.ground_follow,
            "ground": has_ground,
            "ground_m": w.ground_m(),
            "lift_m": w.lift_m,
            "preset": w.preset.map(ComfortPreset::name),
            "budget": budget,
            "aiming": w.aiming(),
            "aim_target": w.aim_target().map(vec3),
            "teleports": w.teleports(),
            "jumps": w.jumps(),
            "distance_m": w.distance_m(),
            "ground_misses": w.ground_misses(),
            "steps": w.steps(),
            "head_world": head.map(vec3),
            "app_state": app_state,
        })
    }

    pub(super) fn merge_walk(world: &mut World, mut out: Value) -> Value {
        if !world.contains_resource::<XrWalk>() {
            return out;
        }
        let walk = walk_json(world);
        if let Value::Object(map) = &mut out {
            map.insert("walk".into(), walk);
        }
        out
    }

    pub(super) fn walk_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        if let Some(r) = refused(world) {
            crate::params::check_types("xr.walk_start", params)?;
            return Ok(r);
        }
        let start = WalkStart {
            position: params.get("position").map(position).transpose()?,
            yaw: params
                .get("yaw")
                .map(|v| finite(v).ok_or_else(|| invalid("yaw", "a finite number of radians")))
                .transpose()?,
            preset: params.get("preset").map(preset).transpose()?,
            budget: match params.get("budget") {
                None => true,
                Some(v) => v.as_bool().ok_or_else(|| invalid("budget", "true or false"))?,
            },
        };
        let focus = match params.get("focus") {
            None => true,
            Some(v) => v.as_bool().ok_or_else(|| invalid("focus", "true or false"))?,
        };
        let simulator = world
            .get_resource::<regia_platform::xr::XrRuntime>()
            .is_some_and(|rt| rt.backend().name() == "null");
        let focused = world
            .get_resource::<XrState>()
            .is_some_and(|s| s.app == XrAppState::Focused);
        if focus && simulator && !focused {
            regia_xr::drive_session_state(world, ScriptState::Focused).map_err(|e| xr_error(&e))?;
        }
        regia_xr::walk_start(world, start).map_err(|e| xr_error(&e))?;
        Ok(walk_json(world))
    }

    pub(super) fn walk_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        if let Some(r) = refused(world) {
            crate::params::check_types("xr.walk_set", params)?;
            return Ok(r);
        }
        if !params.is_object() {
            return Err(RpcError::invalid_params("params must be an object"));
        }
        // Parse and check everything first: a bad value changes nothing.
        let active = params
            .get("active")
            .map(|v| v.as_bool().ok_or_else(|| invalid("active", "true or false")))
            .transpose()?;
        let locomotion = params
            .get("locomotion")
            .map(|v| {
                v.as_str()
                    .and_then(Locomotion::from_name)
                    .ok_or_else(|| invalid("locomotion", "\"smooth\", \"teleport\" or \"off\""))
            })
            .transpose()?;
        let preset = params.get("preset").map(preset).transpose()?;
        let ground_follow = params
            .get("ground_follow")
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| invalid("ground_follow", "true or false"))
            })
            .transpose()?;
        let mut settings = world.resource::<XrWalk>().settings.clone();
        if let Some(v) = params.get("move_speed_mps") {
            settings.move_speed_mps =
                finite(v).ok_or_else(|| invalid("move_speed_mps", "must be 0.1..=6"))?;
        }
        if let Some(v) = params.get("teleport_range_m") {
            settings.teleport_range_m =
                finite(v).ok_or_else(|| invalid("teleport_range_m", "must be 1..=30"))?;
        }
        settings.validate().map_err(|e| xr_error(&e))?;
        let calibrate = match params.get("calibrate_height") {
            None | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(None),
            Some(v) => Some(Some(finite(v).ok_or_else(|| {
                invalid("calibrate_height", "true or an eye height in meters")
            })?)),
        };
        // Calibration is the one change that can fail on state: run it before anything is stored.
        // It runs under the requested `ground_follow` (ADR-5597): with ground follow on,
        // calibration leaves moving the rig to the next walk step, so calibrating under the old
        // `true` and then storing `false` lost the new eye height. A failed calibration puts the
        // old value back, so it still stores nothing.
        if let Some(eye) = calibrate {
            let old_follow = world.resource::<XrWalk>().settings.ground_follow;
            if let Some(gf) = ground_follow {
                world.resource_mut::<XrWalk>().settings.ground_follow = gf;
            }
            if let Err(e) = regia_xr::calibrate_height(world, eye) {
                world.resource_mut::<XrWalk>().settings.ground_follow = old_follow;
                return Err(xr_error(&e));
            }
        }
        if let Some(p) = preset {
            regia_xr::apply_preset(world, p).map_err(|e| xr_error(&e))?;
            settings.locomotion = world.resource::<XrWalk>().settings.locomotion;
        }
        if let Some(l) = locomotion {
            settings.locomotion = l;
        }
        if let Some(gf) = ground_follow {
            settings.ground_follow = gf;
        }
        {
            let mut walk = world.resource_mut::<XrWalk>();
            walk.settings = settings;
            if let Some(a) = active {
                walk.active = a;
            }
        }
        Ok(walk_json(world))
    }
}

#[cfg(all(test, feature = "xr"))]
mod tests {
    use super::*;
    use regia_core::glam::{Quat, Vec3};
    use regia_platform::xr::Pose;
    use regia_scene::Transform;
    use regia_xr::{XrBackendKind, XrConfig, XrGround, XrHead, XrPlugin, XrRig, XrWalkPlugin};

    fn app(walk: bool) -> App {
        let mut app = App::new();
        enable_fixed_step(&mut app, 60.0);
        app.init_resource::<regia_input::InputInjection>();
        app.add_plugins(XrPlugin::new(XrConfig {
            backend: XrBackendKind::Simulator,
            ..XrConfig::default()
        }));
        if walk {
            app.add_plugins(XrWalkPlugin::default());
            app.insert_resource(XrGround::flat(10.0));
            app.world_mut().spawn((XrRig, Transform::IDENTITY));
            app.world_mut().spawn((XrHead, Transform::IDENTITY));
        }
        app
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    #[test]
    fn without_the_plugins_the_methods_are_refused() {
        let mut bare = App::new();
        let got = xr_walk_start(bare.world_mut(), &json!({})).expect("refused");
        assert_eq!(got["refused"], "xr_not_installed");
        let mut no_walk = app(false);
        let got = xr_walk_set(no_walk.world_mut(), &json!({})).expect("refused");
        assert_eq!(got["refused"], "xr_walk_not_installed");
        assert_eq!(got["installed"], true);
        let out = merge_walk(no_walk.world_mut(), json!({}));
        assert!(out.get("walk").is_none());
    }

    #[test]
    fn start_focuses_the_simulator_places_the_head_and_walks() {
        let mut app = app(true);
        let got = xr_walk_start(
            app.world_mut(),
            &json!({ "position": [3.0, 5.0], "yaw": 0.0, "preset": "moderate" }),
        )
        .expect("start");
        assert_eq!(got["active"], true);
        assert_eq!(got["app_state"], "Focused");
        assert_eq!(got["preset"], "moderate");
        assert_eq!(got["locomotion"], "smooth");
        assert!(got["budget"]["render_scale"].as_f64().is_some());
        regia_xr::drive_pose(
            app.world_mut(),
            Some(Pose::new(Vec3::new(0.0, 1.7, 0.0), Quat::IDENTITY)),
            [None, None],
        )
        .expect("pose");
        regia_xr::drive_input(
            app.world_mut(),
            "move",
            "left",
            regia_xr::XrActionValue::Vec2(regia_core::glam::Vec2::new(0.0, 1.0)),
        )
        .expect("move");
        for _ in 0..30 {
            app.update();
        }
        let got = merge_walk(app.world_mut(), json!({}));
        let walk = &got["walk"];
        assert!(walk["distance_m"].as_f64().expect("distance") > 0.5, "{walk}");
        assert_eq!(walk["ground_m"], 10.0);
        let head = walk["head_world"].as_array().expect("head");
        assert!((head[1].as_f64().expect("y") - 11.7).abs() < 1e-4, "{walk}");
        assert!(head[2].as_f64().expect("z") < 5.0, "walked forward (-Z)");
    }

    #[test]
    fn bad_values_are_refused_by_name_and_change_nothing() {
        let mut app = app(true);
        let e = xr_walk_start(app.world_mut(), &json!({ "position": [1.0] })).expect_err("position");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
        let e = xr_walk_start(app.world_mut(), &json!({ "preset": "wild" })).expect_err("preset");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
        let e = xr_walk_set(
            app.world_mut(),
            &json!({ "locomotion": "teleport", "move_speed_mps": 99.0 }),
        )
        .expect_err("speed");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
        let got = merge_walk(app.world_mut(), json!({}));
        assert_eq!(got["walk"]["locomotion"], "smooth", "nothing changed");
        assert!(app.world().resource::<regia_xr::XrWalk>().settings.ground_follow);
        let e = xr_walk_set(
            app.world_mut(),
            &json!({ "ground_follow": false, "calibrate_height": true }),
        )
        .expect_err("idle");
        assert_eq!(reason(&e).as_deref(), Some("xr_not_running"));
        assert!(
            app.world().resource::<regia_xr::XrWalk>().settings.ground_follow,
            "a failed calibration stores nothing, including ground_follow"
        );
    }

    /// corr-agent (ADR-5597): `{ ground_follow: false, calibrate_height }` calibrated under the
    /// old ground follow (which leaves the rig to the next step), then turned ground follow off,
    /// so no step ever applied the new lift: the rig stayed where it was.
    #[test]
    fn calibrating_while_turning_ground_follow_off_moves_the_rig() {
        let mut app = app(true);
        xr_walk_start(app.world_mut(), &json!({})).expect("start");
        regia_xr::drive_pose(
            app.world_mut(),
            Some(Pose::new(Vec3::new(0.0, 1.25, 0.0), Quat::IDENTITY)),
            [None, None],
        )
        .expect("pose");
        app.update();
        let rig_y = |app: &mut App| {
            let mut rigs = app
                .world_mut()
                .query_filtered::<&Transform, regia_ecs::prelude::With<XrRig>>();
            rigs.single(app.world())
                .map(|t| t.translation.y)
                .expect("one rig")
        };
        let before = rig_y(&mut app);
        let old_lift = app.world().resource::<regia_xr::XrWalk>().lift_m;
        let got = xr_walk_set(
            app.world_mut(),
            &json!({ "ground_follow": false, "calibrate_height": 1.65 }),
        )
        .expect("set");
        let new_lift = got["lift_m"].as_f64().expect("lift") as f32;
        assert!(
            (new_lift - old_lift).abs() > 0.1,
            "the calibration changes the lift: {got}"
        );
        assert!(!app.world().resource::<regia_xr::XrWalk>().settings.ground_follow);
        let after = rig_y(&mut app);
        assert!(
            ((after - before) - (new_lift - old_lift)).abs() < 1e-4,
            "the rig carries the new lift: before {before}, after {after}, lift {old_lift} -> {new_lift}"
        );
    }

    #[test]
    fn set_changes_locomotion_speed_and_calibrates() {
        let mut app = app(true);
        xr_walk_start(app.world_mut(), &json!({})).expect("start");
        regia_xr::drive_pose(
            app.world_mut(),
            Some(Pose::new(Vec3::new(0.0, 1.25, 0.0), Quat::IDENTITY)),
            [None, None],
        )
        .expect("pose");
        app.update();
        let got = xr_walk_set(
            app.world_mut(),
            &json!({ "locomotion": "teleport", "move_speed_mps": 2.0, "teleport_range_m": 12.0,
                     "calibrate_height": 1.65 }),
        )
        .expect("set");
        assert_eq!(got["locomotion"], "teleport");
        assert_eq!(got["move_speed_mps"], 2.0);
        assert_eq!(got["teleport_range_m"], 12.0);
        assert!(
            (got["lift_m"].as_f64().expect("lift") - 0.4).abs() < 1e-5,
            "{got}"
        );
        let got = xr_walk_set(app.world_mut(), &json!({ "preset": "free", "active": false })).expect("set");
        assert_eq!(got["locomotion"], "smooth");
        assert_eq!(got["active"], false);
    }

    #[test]
    fn calibration_uses_the_ground_follow_from_the_same_call() {
        let mut app = app(true);
        xr_walk_start(app.world_mut(), &json!({})).expect("start");
        regia_xr::drive_pose(
            app.world_mut(),
            Some(Pose::new(Vec3::new(0.0, 1.25, 0.0), Quat::IDENTITY)),
            [None, None],
        )
        .expect("pose");
        app.update();
        let before = head_y(&mut app);
        let got = xr_walk_set(
            app.world_mut(),
            &json!({ "ground_follow": false, "calibrate_height": 1.65 }),
        )
        .expect("set");
        assert!(
            (got["lift_m"].as_f64().expect("lift") - 0.4).abs() < 1e-5,
            "{got}"
        );
        assert_eq!(got["ground_follow"], false);
        let after = head_y(&mut app);
        assert!(
            (after - before - 0.4).abs() < 1e-4,
            "turning ground follow off must carry the lift on the rig, before {before} after {after}"
        );

        // A taller eye height while turning follow back on must wait for the stepper.
        let parked = head_y(&mut app);
        let got = xr_walk_set(
            app.world_mut(),
            &json!({ "ground_follow": true, "calibrate_height": 2.0 }),
        )
        .expect("set");
        assert_eq!(got["ground_follow"], true);
        assert!(
            (got["lift_m"].as_f64().expect("lift") - 0.75).abs() < 1e-5,
            "{got}"
        );
        let stayed = head_y(&mut app);
        assert!(
            (stayed - parked).abs() < 1e-4,
            "turning ground follow on leaves the rig where it is, parked {parked} now {stayed}"
        );
    }

    fn head_y(app: &mut App) -> f64 {
        merge_walk(app.world_mut(), json!({}))["walk"]["head_world"][1]
            .as_f64()
            .expect("head world y")
    }
}
