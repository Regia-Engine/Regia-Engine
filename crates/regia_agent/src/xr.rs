// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the OpenXR seam (ADR-1571, book `agents/xr.md`): `xr.get` reports the
//! session, the last frame, the late latch and the controllers; `xr.drive` moves a scripted
//! backend through the session states, runs frames, moves the head and plays haptics.

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec3};
use regia_platform::xr::{Hand, Pose, SessionState, SimEvent, XrError, XrEvent, XrRuntime};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most frames one `xr.drive` `frame` call runs.
const MAX_DRIVE_FRAMES: u64 = 1000;

/// `refused` code when no [`XrRuntime`] resource is installed.
const NOT_INSTALLED: &str = "xr_not_installed";

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn pose_json(p: &Pose) -> Value {
    let q = p.orientation;
    json!({ "position": vec3_json(p.position), "orientation": [q.x, q.y, q.z, q.w] })
}

fn event_json(event: &XrEvent) -> Value {
    match event {
        XrEvent::StateChanged { state } => json!({ "kind": "state_changed", "state": state.name() }),
        XrEvent::ControllerActive { hand, active } => {
            json!({ "kind": "controller_active", "hand": hand.name(), "active": active })
        }
        XrEvent::FocusChanged { focused } => json!({ "kind": "focus_changed", "focused": focused }),
        XrEvent::VisibilityMaskChanged => json!({ "kind": "visibility_mask_changed" }),
    }
}

fn runtime_json(rt: &XrRuntime) -> Value {
    let backend = rt.backend();
    let head = backend.head_sample();
    let controllers: Vec<Value> = Hand::BOTH
        .iter()
        .map(|hand| {
            let c = backend.controller(*hand);
            json!({
                "hand": hand.name(),
                "active": c.active,
                "grip": pose_json(&c.grip.pose),
                "aim": pose_json(&c.aim.pose),
                "trigger": c.trigger,
                "squeeze": c.squeeze,
                "thumbstick": [c.thumbstick.x, c.thumbstick.y],
                "primary": c.primary,
                "secondary": c.secondary,
                "thumbstick_click": c.thumbstick_click,
                "menu": c.menu,
            })
        })
        .collect();
    let last = rt.last_report().map(|r| {
        json!({
            "frame_index": r.timing.frame_index,
            "display_time_ns": r.timing.predicted_display_time_ns,
            "display_period_ns": r.timing.predicted_display_period_ns,
            "rendered": r.rendered,
            "clamped": r.clamped,
            "views": r.views.iter().map(|v| json!({
                "pose": pose_json(&v.pose),
                "fov": { "left": v.fov.left, "right": v.fov.right, "up": v.fov.up, "down": v.fov.down },
            })).collect::<Vec<_>>(),
        })
    });
    json!({
        "installed": true,
        "backend": backend.name(),
        "state": backend.state().name(),
        "frames": rt.frames(),
        "last_frame": last,
        "latch": {
            "latched": rt.latch().latched(),
            "clamped": rt.latch().clamped(),
            "max_extrapolation_ns": rt.latch().max_extrapolation_ns,
        },
        "head": pose_json(&head.pose),
        "controllers": controllers,
        "events": rt.events().iter().map(event_json).collect::<Vec<_>>(),
    })
}

fn xr_error(err: &XrError) -> RpcError {
    let mut e = RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "F0002",
        err.to_string(),
        "xr.get shows the session state; drive it ready -> begin -> frame -> exit -> end",
    );
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(err.code()));
    }
    e
}

/// `xr.get`: the XR runtime, or `{ installed: false }`. With the `regia_xr` plugin it also carries
/// the spec 58 fields (`app_state`, `backend_kind`, options, `stats`, `last_error`, ...); while the
/// plugin has no session (off, no headset, a lost runtime) only those are present.
pub(crate) fn xr_get(world: &mut World) -> Result<Value, RpcError> {
    let out = world
        .get_resource::<XrRuntime>()
        .map_or_else(|| json!({ "installed": false }), runtime_json);
    #[cfg(feature = "xr")]
    let out = ext::merge_state(world, out);
    let out = crate::xr_walk::merge_walk(world, out);
    Ok(out)
}

/// A number that is finite as `f32` (a finite `f64` past `f32::MAX` is refused, not made
/// infinite).
fn finite_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|v| v as f32).filter(|v| v.is_finite())
}

fn parse_vec3(value: Option<&Value>) -> Option<Vec3> {
    let a = value?.as_array()?;
    if a.len() != 3 {
        return None;
    }
    let f = |i: usize| finite_f32(&a[i]);
    Some(Vec3::new(f(0)?, f(1)?, f(2)?))
}

/// `xr.drive { action, ... }`: `ready`, `begin`, `exit`, `stop`, `end`, `frame { count }`,
/// `head { position, yaw }`, `haptic { hand, amplitude, duration_ms }`. Without a runtime the
/// answer carries `refused: "xr_not_installed"`.
pub(crate) fn xr_drive(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RpcError::invalid_params(
                "`action` is required (ready, begin, exit, stop, end, frame, head, haptic, pose, input, session_state, lens, mono, frame_time)",
            )
        })?
        .to_string();
    if matches!(
        action.as_str(),
        "pose" | "input" | "session_state" | "lens" | "mono" | "frame_time"
    ) {
        return drive_sim(world, &action, params);
    }
    let Some(mut rt) = world.get_resource_mut::<XrRuntime>() else {
        return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
    };
    let result: Result<(), XrError> = match action.as_str() {
        "ready" => rt.backend_mut().simulate(SimEvent::RuntimeReady),
        "begin" => rt.backend_mut().begin_session(),
        "exit" => rt.backend_mut().request_exit(),
        "stop" => rt.backend_mut().simulate(SimEvent::RuntimeStop),
        "end" => rt.backend_mut().end_session(),
        "frame" => {
            let count = match params.get("count") {
                None => 1,
                Some(v) => v
                    .as_u64()
                    .filter(|c| (1..=MAX_DRIVE_FRAMES).contains(c))
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("`count` must be an integer 1..={MAX_DRIVE_FRAMES}"))
                    })?,
            };
            (0..count).try_for_each(|_| rt.frame(|_, _| {}).map(|_| ()))
        }
        "head" => {
            let position = parse_vec3(params.get("position"))
                .ok_or_else(|| RpcError::invalid_params("`position` must be [x, y, z] in meters"))?;
            let yaw = match params.get("yaw") {
                None | Some(Value::Null) => 0.0,
                Some(v) => finite_f32(v)
                    .ok_or_else(|| RpcError::invalid_params("`yaw` must be a finite number of radians"))?,
            };
            let pose = Pose::new(position, Quat::from_rotation_y(yaw));
            rt.backend_mut().simulate(SimEvent::HeadPose(pose))
        }
        "haptic" => {
            let hand = match crate::params::opt_str(params, "hand")? {
                Some("left") => Hand::Left,
                Some("right") => Hand::Right,
                _ => return Err(RpcError::invalid_params("`hand` must be \"left\" or \"right\"")),
            };
            let amplitude = crate::params::opt_f64(params, "amplitude")?.unwrap_or(1.0) as f32;
            let ms = match params.get("duration_ms") {
                None | Some(Value::Null) => 100.0,
                Some(v) => v
                    .as_f64()
                    .filter(|ms| ms.is_finite() && *ms >= 0.0)
                    .ok_or_else(|| {
                        RpcError::invalid_params("`duration_ms` must be a finite number, 0 or more")
                    })?,
            };
            rt.backend_mut()
                .apply_haptic(hand, amplitude, (ms * 1.0e6) as i64)
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown action `{other}` (ready, begin, exit, stop, end, frame, head, haptic, pose, input, session_state)"
            )));
        }
    };
    rt.pump();
    let state: SessionState = rt.backend().state();
    result.map_err(|e| xr_error(&e))?;
    Ok(json!({ "ok": true, "action": action, "state": state.name(), "frames": rt.frames() }))
}

fn parse_quat(value: Option<&Value>) -> Option<Quat> {
    let a = value?.as_array()?;
    if a.len() != 4 {
        return None;
    }
    let f = |i: usize| finite_f32(&a[i]);
    Some(Quat::from_xyzw(f(0)?, f(1)?, f(2)?, f(3)?))
}

/// A pose parameter: `{ position: [x, y, z], orientation?: [x, y, z, w], yaw? }` (radians about
/// +Y when no orientation is given). `Ok(None)` when absent.
fn parse_pose(params: &Value, key: &str) -> Result<Option<Pose>, RpcError> {
    let Some(v) = params.get(key) else {
        return Ok(None);
    };
    let bad = || {
        RpcError::invalid_params(format!(
            "`{key}` must be {{ position: [x, y, z], orientation?: [x, y, z, w], yaw? }} with finite numbers"
        ))
    };
    let position = parse_vec3(v.get("position")).ok_or_else(bad)?;
    let orientation = match (v.get("orientation"), v.get("yaw")) {
        (Some(q), _) => parse_quat(Some(q)).ok_or_else(bad)?,
        (None, Some(y)) => Quat::from_rotation_y(finite_f32(y).ok_or_else(bad)?),
        (None, None) => Quat::IDENTITY,
    };
    Ok(Some(Pose::new(position, orientation)))
}

/// `xr.drive` `pose`, `input`, `session_state` (spec 58 § 5.1), `lens` and `mono`: simulator-only
/// drives.
fn drive_sim(world: &mut World, action: &str, params: &Value) -> Result<Value, RpcError> {
    if !world.contains_resource::<XrRuntime>() && !has_plugin(world) {
        return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
    }
    let result = match action {
        "pose" => drive_pose(world, params),
        "input" => drive_input(world, params),
        "lens" => drive_lens(world, params),
        "mono" => drive_mono(world, params),
        "frame_time" => drive_frame_time(world, params),
        _ => drive_state(world, params),
    };
    let state = world
        .get_resource::<XrRuntime>()
        .map_or("none", |rt| rt.backend().state().name());
    result?;
    Ok(json!({ "ok": true, "action": action, "state": state, "app_state": app_state_name(world) }))
}

#[cfg(feature = "xr")]
fn app_state_name(world: &World) -> Option<&'static str> {
    world.get_resource::<regia_xr::XrState>().map(|s| s.app.name())
}

#[cfg(not(feature = "xr"))]
fn app_state_name(_world: &World) -> Option<&'static str> {
    None
}

#[cfg(feature = "xr")]
fn has_plugin(world: &World) -> bool {
    world.contains_resource::<regia_xr::XrSession>()
}

#[cfg(not(feature = "xr"))]
fn has_plugin(_world: &World) -> bool {
    false
}

fn simulator_check(world: &World, call: &'static str) -> Result<(), RpcError> {
    match world.get_resource::<XrRuntime>() {
        Some(rt) if rt.backend().name() != "null" => Err(xr_error(&XrError::SimulatorOnly { call })),
        Some(_) => Ok(()),
        None => Err(xr_error(&XrError::NotRunning {
            call,
            state: SessionState::Idle,
        })),
    }
}

/// `lens { radius }`: the scripted visibility mask, 0 for none.
fn drive_lens(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let radius = params
        .get("radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| RpcError::invalid_params("`radius` is required (0 for no mask, or 0.1..=2)"))?;
    simulator_check(world, "lens")?;
    world
        .resource_mut::<XrRuntime>()
        .backend_mut()
        .simulate(SimEvent::LensMask {
            radius: radius as f32,
        })
        .map_err(|e| xr_error(&e))
}

/// `frame_time { ms }`: the simulator's frame time for every following XR frame, milliseconds
/// (finite, 0 or more), or `null` to stop measuring simulator frames (spec 58 § 10: the frame
/// budget, `xr_over_budget` and `auto_scale` read it; the simulator never reads the wall clock).
fn drive_frame_time(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let bad =
        || RpcError::invalid_params("`ms` is required: a finite number of milliseconds, 0 or more, or null");
    let ms = match params.get("ms") {
        Some(Value::Null) => None,
        Some(v) => Some(finite_f32(v).filter(|ms| *ms >= 0.0).ok_or_else(bad)?),
        None => return Err(bad()),
    };
    simulator_check(world, "frame_time")?;
    #[cfg(feature = "xr")]
    if let Some(mut budget) = world.get_resource_mut::<regia_xr::XrFrameBudget>() {
        budget.script_frame_ms(ms);
        return Ok(());
    }
    let _ = ms;
    Err(xr_error(&XrError::Unsupported(
        "frame_time without the regia_xr plugin",
    )))
}

/// `mono { on }`: both eyes at the head, the zero-disparity check.
fn drive_mono(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let on = params
        .get("on")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`on` is required (true or false)"))?;
    simulator_check(world, "mono")?;
    world
        .resource_mut::<XrRuntime>()
        .backend_mut()
        .simulate(SimEvent::MonoEyes(on))
        .map_err(|e| xr_error(&e))
}

fn drive_pose(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let head = parse_pose(params, "head")?;
    let left = parse_pose(params, "left")?;
    let right = parse_pose(params, "right")?;
    let fov_deg = params.get("fov_deg");
    let ipd = params.get("ipd_m");
    simulator_check(world, "pose")?;
    if fov_deg.is_some() || ipd.is_some() {
        let num = |v: Option<&Value>, what: &str| -> Result<Option<f32>, RpcError> {
            match v {
                None => Ok(None),
                Some(v) => v
                    .as_f64()
                    .filter(|x| x.is_finite())
                    .map(|x| Some(x as f32))
                    .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be a number"))),
            }
        };
        let fov_deg = num(fov_deg, "fov_deg")?;
        let ipd_m = num(ipd, "ipd_m")?;
        // A field that was not sent keeps the simulator's current value.
        let (cur_ipd, cur_fov) = world.resource::<XrRuntime>().backend().view_config().unwrap_or((
            regia_platform::xr::DEFAULT_IPD_M,
            regia_platform::xr::Fov::symmetric(50.0_f32.to_radians(), 50.0_f32.to_radians()),
        ));
        let fov = fov_deg.map_or(cur_fov, |d| {
            let half = (d * 0.5).to_radians();
            regia_platform::xr::Fov::symmetric(half, half)
        });
        let ipd_m = ipd_m.unwrap_or(cur_ipd);
        world
            .resource_mut::<XrRuntime>()
            .backend_mut()
            .simulate(SimEvent::ViewConfig { ipd_m, fov })
            .map_err(|e| xr_error(&e))?;
    }
    #[cfg(feature = "xr")]
    if has_plugin(world) {
        return regia_xr::drive_pose(world, head, [left, right]).map_err(|e| xr_error(&e));
    }
    let mut rt = world.resource_mut::<XrRuntime>();
    if let Some(h) = head {
        rt.backend_mut()
            .simulate(SimEvent::HeadPose(h))
            .map_err(|e| xr_error(&e))?;
    }
    for (hand, pose) in [(Hand::Left, left), (Hand::Right, right)] {
        if let Some(p) = pose {
            let c = regia_platform::xr::ControllerState {
                active: true,
                grip: regia_platform::xr::PoseSample::at_rest(p, 0),
                aim: regia_platform::xr::PoseSample::at_rest(p, 0),
                ..rt.backend().controller(hand)
            };
            rt.backend_mut()
                .simulate(SimEvent::Controller(c))
                .map_err(|e| xr_error(&e))?;
        }
    }
    Ok(())
}

fn drive_input(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let name = params
        .get("action_name")
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`name` (the manifest action) is required"))?
        .to_string();
    let sub = crate::params::opt_str(params, "sub")?.unwrap_or("").to_string();
    let value = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required (bool, number or [x, y])"))?
        .clone();
    simulator_check(world, "input")?;
    #[cfg(feature = "xr")]
    {
        use regia_xr::XrActionValue;
        if !has_plugin(world) {
            return Err(xr_error(&XrError::Unsupported("input without the xr plugin")));
        }
        let v = match &value {
            Value::Bool(b) => XrActionValue::Bool(*b),
            Value::Number(n) => XrActionValue::Float(n.as_f64().unwrap_or(0.0) as f32),
            Value::Array(a) if a.len() == 2 => {
                let f = |i: usize| a[i].as_f64().filter(|x| x.is_finite()).map(|x| x as f32);
                match (f(0), f(1)) {
                    (Some(x), Some(y)) => XrActionValue::Vec2(regia_core::glam::Vec2::new(x, y)),
                    _ => return Err(RpcError::invalid_params("`value` [x, y] must be finite numbers")),
                }
            }
            _ => {
                return Err(RpcError::invalid_params(
                    "`value` must be a bool, a number or [x, y]",
                ));
            }
        };
        regia_xr::drive_input(world, &name, &sub, v).map_err(|e| xr_error(&e))
    }
    #[cfg(not(feature = "xr"))]
    {
        let _ = (name, sub, value);
        Err(xr_error(&XrError::Unsupported("input without the xr feature")))
    }
}

fn drive_state(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let name = params.get("state").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(
            "`state` is required (ready, synchronized, visible, focused, stopping, loss_pending, exiting)",
        )
    })?;
    let target = SessionState::from_name(name)
        .filter(|s| !matches!(s, SessionState::Idle | SessionState::Running))
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown state `{name}` (ready, synchronized, visible, focused, stopping, loss_pending, exiting)"
            ))
        })?;
    simulator_check(world, "session_state")?;
    #[cfg(feature = "xr")]
    if has_plugin(world) {
        let s = regia_xr::sim::ScriptState::from_name(target.name())
            .ok_or_else(|| RpcError::invalid_params("unknown state"))?;
        return regia_xr::drive_session_state(world, s)
            .map(|_| ())
            .map_err(|e| xr_error(&e));
    }
    // No plugin: walk the bare backend (no begin/end reactions beyond the path itself).
    let mut rt = world.resource_mut::<XrRuntime>();
    let path = session_path(rt.backend().state(), target).ok_or_else(|| {
        xr_error(&XrError::InvalidTransition {
            from: rt.backend().state(),
            transition: regia_platform::xr::Transition::RuntimeReady,
        })
    })?;
    for t in path {
        let r = match t {
            regia_platform::xr::Transition::Begin => rt.backend_mut().begin_session(),
            regia_platform::xr::Transition::End => rt.backend_mut().end_session(),
            other => rt.backend_mut().simulate(SimEvent::Runtime(other)),
        };
        r.map_err(|e| xr_error(&e))?;
    }
    rt.pump();
    Ok(())
}

/// Shortest valid transition path between two states (breadth first over every transition except
/// `RequestExit`), for the plugin-less drive.
fn session_path(from: SessionState, to: SessionState) -> Option<Vec<regia_platform::xr::Transition>> {
    use regia_platform::xr::{SessionCore, Transition};
    let reach = |path: &[Transition]| {
        let mut core = SessionCore::new();
        for t in path {
            core.transition(*t).ok()?;
        }
        Some(core)
    };
    // Paths are found from Idle; `from` is located first, then extended.
    let mut queue: std::collections::VecDeque<Vec<Transition>> =
        std::collections::VecDeque::from([Vec::new()]);
    let mut prefix = None;
    let mut seen = Vec::new();
    while let Some(path) = queue.pop_front() {
        let Some(core) = reach(&path) else { continue };
        let state = core.state();
        if prefix.is_none() && state == from {
            prefix = Some(path.len());
            queue.clear();
            seen.clear();
        }
        if prefix.is_some() && state == to && path.len() >= prefix.unwrap_or(0) {
            return Some(path[prefix.unwrap_or(0)..].to_vec());
        }
        if seen.contains(&state) {
            continue;
        }
        seen.push(state);
        for t in Transition::ALL {
            if t == Transition::RequestExit {
                continue;
            }
            let mut next = path.clone();
            next.push(t);
            if reach(&next).is_some() {
                queue.push_back(next);
            }
        }
    }
    None
}

#[cfg(feature = "xr")]
mod ext {
    //! The spec 58 methods over `regia_xr` (book `agents/xr.md`).
    use super::*;
    use regia_xr::{AppliedAt, OptionValue, XrConfig, XrSession, XrState};

    pub(super) fn merge_state(world: &World, mut out: Value) -> Value {
        let Some(st) = world.get_resource::<XrState>() else {
            return out;
        };
        let config = world.get_resource::<XrConfig>();
        let session = world.get_resource::<XrSession>();
        let Value::Object(map) = &mut out else {
            return out;
        };
        map.insert("installed".into(), json!(true));
        map.insert("backend_kind".into(), json!(st.backend_kind.name()));
        if !map.contains_key("backend") {
            map.insert("backend".into(), json!(st.backend));
        }
        map.insert("app_state".into(), json!(st.app.name()));
        map.insert("session_state".into(), json!(st.session.name()));
        map.insert("runtime".into(), json!(st.runtime));
        map.insert("system".into(), json!(st.system));
        map.insert("tracking_space".into(), json!(st.tracking_space.name()));
        map.insert(
            "actual_tracking_space".into(),
            json!(st.actual_tracking_space.name()),
        );
        map.insert("refresh_hz".into(), json!(st.refresh_hz));
        map.insert("render_scale".into(), json!(st.render_scale));
        map.insert("foveation".into(), json!(st.foveation.name()));
        map.insert("mirror".into(), json!(st.mirror.name()));
        map.insert("extensions_enabled".into(), json!(st.extensions_enabled));
        map.insert("hand_tracking".into(), json!(st.hand_tracking.name()));
        map.insert(
            "play_area".into(),
            st.play_area.map_or(
                Value::Null,
                |p| json!({ "width_m": p.width_m, "depth_m": p.depth_m }),
            ),
        );
        map.insert(
            "stats".into(),
            json!({
                "frames": st.stats.frames,
                "stale_frames": st.stats.stale_frames,
                "bad_poses": st.stats.bad_poses,
                "dropped_frames": st.stats.dropped_frames,
                "over_budget": st.stats.over_budget,
            }),
        );
        if let Some(b) = world.get_resource::<regia_xr::XrFrameBudget>() {
            map.insert(
                "budget".into(),
                json!({
                    "budget_ms": regia_xr::budget::budget_ms_for(st.refresh_hz),
                    "frames": b.frames,
                    "auto_scale": b.auto_scale,
                    "over_run": b.over_run(),
                    "last_frame_ms": b.last_frame_ms(),
                    "scripted_ms": b.scripted_ms(),
                    "last": b.last().map_or(Value::Null, |e| json!({
                        "code": regia_xr::budget::OVER_BUDGET_CODE,
                        "frame_ms": e.frame_ms,
                        "budget_ms": e.budget_ms,
                        "frames": e.frames,
                        "scale_from": e.scale_from,
                        "scale_to": e.scale_to,
                    })),
                }),
            );
        }
        map.insert(
            "last_error".into(),
            st.last_error.as_ref().map_or(
                Value::Null,
                |(code, message)| json!({ "code": code, "message": message }),
            ),
        );
        map.insert("retry_in".into(), json!(st.retry_in));
        map.insert("sessions_created".into(), json!(st.sessions_created));
        if let Some(c) = config {
            map.insert(
                "comfort".into(),
                json!({
                    "snap_turn_deg": c.comfort.snap_turn_deg,
                    "vignette_strength": c.comfort.vignette_strength,
                    "eye_height_m": c.comfort.eye_height_m,
                }),
            );
        }
        if let Some(s) = session {
            map.insert("offset".into(), pose_json(&s.offset));
            map.insert("held_head".into(), pose_json(&s.held_head()));
            map.insert("vignette".into(), json!(s.vignette()));
            map.insert("snaps".into(), json!(s.snaps()));
            map.insert("ticks".into(), json!(s.tick()));
            map.insert("actions_attached".into(), json!(s.actions_attached()));
        }
        if let Some(states) = world.get_resource::<regia_xr::XrActionStates>() {
            let actions: Vec<Value> = states
                .iter()
                .map(|((name, sub), active, value)| {
                    let v = match value {
                        regia_xr::XrActionValue::Bool(b) => json!(b),
                        regia_xr::XrActionValue::Float(f) => json!(f),
                        regia_xr::XrActionValue::Vec2(v) => json!([v.x, v.y]),
                        regia_xr::XrActionValue::Pose(p) => pose_json(&p),
                    };
                    json!({ "name": name, "sub": sub, "active": active, "value": v })
                })
                .collect();
            map.insert("actions".into(), json!(actions));
        }
        if let Some(render) = render_json(world) {
            map.insert("render".into(), render);
        }
        out
    }

    /// The stereo strategy, the last frame's counters and what the XR profile turns off.
    fn render_json(world: &World) -> Option<Value> {
        let state = world.get_resource::<regia_xr::XrRenderState>()?;
        let profile = world.get_resource::<regia_xr::XrRenderProfile>()?;
        let stats = state.stats();
        let disabled: Vec<Value> = regia_xr::XR_DISABLED_EFFECTS
            .iter()
            .map(|(effect, reason)| json!({ "effect": effect, "reason": reason }))
            .collect();
        Some(json!({
            "stereo_mode": profile.stereo_mode.name(),
            "mode": stats.mode.name(),
            "multiview_supported": stats.multiview_supported,
            "multiview": profile.multiview,
            "extent": [profile.extent.x, profile.extent.y],
            "per_eye_history": profile.per_eye_history,
            "hidden_area": profile.hidden_area,
            "depth_layer": profile.depth_layer,
            "frames": stats.frames,
            "scene_passes": stats.scene_passes,
            "drawn": stats.drawn,
            "hidden_triangles": stats.hidden_triangles,
            "depth_layers": stats.depth_layers,
            "quads_composed": stats.quads_composed,
            "quads_overflow": stats.quads_overflow,
            "last_cpu_ms": stats.last_cpu_ms,
            "disabled_effects": disabled,
        }))
    }

    fn option_json(v: &OptionValue) -> Value {
        match v {
            OptionValue::Bool(b) => json!(b),
            OptionValue::Number(n) => json!(n),
            OptionValue::Text(t) => json!(t),
        }
    }

    fn parse_option(v: &Value) -> Option<OptionValue> {
        match v {
            Value::Bool(b) => Some(OptionValue::Bool(*b)),
            Value::Number(n) => n.as_f64().map(OptionValue::Number),
            Value::String(s) => Some(OptionValue::Text(s.clone())),
            _ => None,
        }
    }

    pub(super) fn set_option(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        let key = params
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("`key` is required"))?
            .to_string();
        let value = params
            .get("value")
            .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
        if !world.contains_resource::<XrSession>() {
            return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
        }
        let old = regia_xr::option_value(world, &key).map_err(|e| xr_error(&e))?;
        let parsed = parse_option(value).ok_or_else(|| {
            xr_error(&XrError::InvalidValue {
                key: key.clone(),
                reason: "a bool, number or string".into(),
            })
        })?;
        let applied: AppliedAt = regia_xr::set_option(world, &key, &parsed).map_err(|e| xr_error(&e))?;
        let new = regia_xr::option_value(world, &key).map_err(|e| xr_error(&e))?;
        Ok(json!({
            "key": key,
            "old": option_json(&old),
            "new": option_json(&new),
            "applied_at": applied.name(),
        }))
    }

    pub(super) fn recenter(world: &mut World) -> Result<Value, RpcError> {
        if !world.contains_resource::<XrSession>() {
            return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
        }
        let offset = regia_xr::recenter(world).map_err(|e| xr_error(&e))?;
        Ok(json!({ "offset": pose_json(&offset) }))
    }

    /// Largest `width` / `height` an eye capture is resized to.
    const MAX_CAPTURE: u64 = 4096;

    pub(super) fn capture_eye(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        use regia_platform::xr::Eye;
        let eyes: Vec<Eye> = match params.get("eye") {
            Some(Value::String(s)) if s == "both" => Eye::BOTH.to_vec(),
            Some(v) => match v.as_u64() {
                Some(0) => vec![Eye::Left],
                Some(1) => vec![Eye::Right],
                _ => return Err(RpcError::invalid_params("`eye` must be 0, 1 or \"both\"")),
            },
            None => return Err(RpcError::invalid_params("`eye` is required (0, 1 or \"both\")")),
        };
        let size = |k: &str| -> Result<Option<u32>, RpcError> {
            match params.get(k) {
                None => Ok(None),
                Some(v) => v
                    .as_u64()
                    .filter(|n| (1..=MAX_CAPTURE).contains(n))
                    .map(|n| Some(n as u32))
                    .ok_or_else(|| RpcError::invalid_params(format!("`{k}` must be 1..={MAX_CAPTURE}"))),
            }
        };
        let (width, height) = (size("width")?, size("height")?);
        let source = params.get("source").map_or(Some("eye"), Value::as_str);
        let source = source
            .filter(|s| matches!(*s, "eye" | "display" | "mirror"))
            .ok_or_else(|| RpcError::invalid_params("`source` must be \"eye\", \"display\" or \"mirror\""))?;
        let Some(st) = world.get_resource::<XrState>() else {
            return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
        };
        if !st.app.is_running() {
            return Err(xr_error(&XrError::NotRunning {
                call: "capture_eye",
                state: st.session,
            }));
        }
        let Some(views) = regia_xr::last_views(world) else {
            return Err(xr_error(&XrError::NotRunning {
                call: "capture_eye (no frame yet)",
                state: st.session,
            }));
        };
        if source == "mirror" {
            return capture_mirror(world, width, height);
        }
        let near = world.get_resource::<XrConfig>().map_or(0.05, |c| c.near_m);
        let targets = world.get_resource::<regia_xr::SimEyeTargets>();
        let render = world.get_resource::<regia_xr::XrRenderState>();
        let device = world.get_resource::<regia_render::RenderDevice>();
        let mut out = Vec::new();
        for eye in eyes {
            let v = views[eye.index()];
            let projection = regia_xr::eye_projection(&v.fov, near).map_err(|e| xr_error(&e))?;
            let mut entry = json!({
                "eye": eye.index(),
                "pose": pose_json(&v.pose),
                "fov": { "left": v.fov.left, "right": v.fov.right, "up": v.fov.up, "down": v.fov.down },
                "view_projection": (projection * v.view_matrix()).to_cols_array().to_vec(),
                "image": Value::Null,
            });
            if let Some(t) = targets {
                use regia_xr::EyeTargets;
                let extent = t.extent();
                let shown = render
                    .and_then(|r| r.display_texture(eye))
                    .filter(|_| source == "display");
                let rgba = match (shown, device) {
                    (Some(texture), Some(device)) => {
                        regia_xr::eye_targets::read_rgba8(device, texture, extent)
                    }
                    _ => t.read_rgba8(eye),
                }
                .map_err(|e| xr_error(&e))?;
                let (w, h) = (width.unwrap_or(extent.x), height.unwrap_or(extent.y));
                let rgba = if (w, h) == (extent.x, extent.y) {
                    rgba
                } else {
                    regia_xr::box_resize(&rgba, extent.x, extent.y, w, h)
                        .ok_or_else(|| RpcError::invalid_params("bad capture size"))?
                };
                let image = regia_render::CapturedImage {
                    width: w,
                    height: h,
                    rgba,
                };
                let png = crate::image::png_base64(&image).map_err(RpcError::internal)?;
                entry["image"] = json!({ "width": w, "height": h, "format": "png", "base64": png });
            }
            out.push(entry);
        }
        let image_source = match (targets.is_some(), source) {
            (false, _) => "none",
            (true, "display") if render.is_some_and(|r| r.display_texture(Eye::Left).is_some()) => "display",
            (true, _) => "eye_targets",
        };
        Ok(json!({ "eyes": out, "image_source": image_source }))
    }

    /// Default size of a mirror capture.
    const MIRROR_CAPTURE: (u32, u32) = (640, 360);

    /// Paints the mirror into a temporary target of the requested size and reads it back.
    fn capture_mirror(world: &World, width: Option<u32>, height: Option<u32>) -> Result<Value, RpcError> {
        use regia_render::OverlayDraw;
        let (Some(device), Some(render)) = (
            world.get_resource::<regia_render::RenderDevice>(),
            world.get_resource::<regia_xr::XrRenderState>(),
        ) else {
            return Ok(json!({ "eyes": [], "image_source": "none", "mirror": Value::Null }));
        };
        let (w, h) = (
            width.unwrap_or(MIRROR_CAPTURE.0),
            height.unwrap_or(MIRROR_CAPTURE.1),
        );
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("xr mirror capture"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xr mirror capture"),
            });
        let mut painter = regia_xr::render::MirrorPainter::new(render.mirror().clone());
        painter.draw(device, &mut encoder, &view, format, w, h);
        device.queue.submit([encoder.finish()]);
        let rgba = regia_xr::eye_targets::read_rgba8(device, &texture, regia_core::glam::UVec2::new(w, h))
            .map_err(|e| xr_error(&e))?;
        let image = regia_render::CapturedImage {
            width: w,
            height: h,
            rgba,
        };
        let png = crate::image::png_base64(&image).map_err(RpcError::internal)?;
        Ok(json!({
            "eyes": [],
            "image_source": "mirror",
            "mirror": { "width": w, "height": h, "format": "png", "base64": png },
        }))
    }

    fn pose_of(world: &World, id: u32) -> Option<regia_platform::xr::Pose> {
        world
            .get_resource::<regia_xr::XrPanels>()?
            .get(id)
            .map(|p| p.pose)
    }

    fn sub_image_json(image: &regia_platform::xr::SubImage) -> Value {
        json!({ "array_index": image.array_index, "rect": image.rect })
    }

    /// `xr.layers {}`: the last frame's composition layers and the runtime's budget.
    pub(super) fn layers(world: &World) -> Result<Value, RpcError> {
        let Some(rt) = world.get_resource::<XrRuntime>() else {
            return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
        };
        let list = rt.last_layers();
        let projection = list.projection.as_ref().map(|layer| {
            let views: Vec<Value> = layer
                .views
                .iter()
                .map(|v| {
                    json!({
                        "pose": pose_json(&v.pose),
                        "fov": { "left": v.fov.left, "right": v.fov.right, "up": v.fov.up, "down": v.fov.down },
                        "image": sub_image_json(&v.image),
                        "depth": v.depth.map(|d| json!({
                            "image": sub_image_json(&d.image),
                            "min_depth": d.min_depth,
                            "max_depth": d.max_depth,
                            "near_m": d.near_m,
                            "far_m": d.far_m,
                        })),
                    })
                })
                .collect();
            json!({ "views": views })
        });
        let quads: Vec<Value> = list
            .quads
            .iter()
            .map(|q| {
                json!({
                    "id": q.id,
                    "priority": q.priority,
                    "space": q.space.name(),
                    "pose": pose_json(&q.pose),
                    "size_m": q.size_m,
                    "image": sub_image_json(&q.image),
                })
            })
            .collect();
        Ok(json!({
            "installed": true,
            "count": list.len(),
            "max_layers": rt.backend().max_layers(),
            "depth_layer_supported": rt.backend().supports_depth_layer(),
            "projection": projection,
            "quads": quads,
        }))
    }

    /// `xr.set_quad { id, remove?, space?, position?, yaw?, size_m?, priority?, color? }`.
    pub(super) fn set_quad(world: &mut World, params: &Value) -> Result<Value, RpcError> {
        use regia_platform::xr::LayerSpace;
        use regia_xr::render::{MAX_PANELS, PanelImage};
        let id = crate::params::opt_u64(params, "id")?
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| RpcError::invalid_params("`id` is required (a 32-bit unsigned integer)"))?;
        if !world.contains_resource::<regia_xr::XrPanels>() {
            return Ok(json!({ "refused": NOT_INSTALLED, "installed": false }));
        }
        if crate::params::opt_bool(params, "remove")? == Some(true) {
            let removed = world.resource_mut::<regia_xr::XrPanels>().remove(id);
            return Ok(json!({ "id": id, "removed": removed }));
        }
        let space = match params.get("space") {
            None => LayerSpace::Rig,
            Some(v) => v
                .as_str()
                .and_then(LayerSpace::from_name)
                .ok_or_else(|| RpcError::invalid_params("`space` must be head, rig or world"))?,
        };
        let mut pose = pose_of(world, id).unwrap_or(regia_platform::xr::Pose {
            position: regia_core::glam::Vec3::new(0.0, 1.5, -1.5),
            ..regia_platform::xr::Pose::IDENTITY
        });
        if params.get("position").is_some()
            && let Some(parsed) = parse_pose(&json!({ "panel": params }), "panel")?
        {
            pose = parsed;
        }
        let pair = |v: &Value| -> Option<[f32; 2]> {
            let a = v.as_array()?;
            match a.as_slice() {
                [x, y] => Some([x.as_f64()? as f32, y.as_f64()? as f32]),
                _ => None,
            }
        };
        let size_m = match params.get("size_m") {
            None => [1.0, 0.6],
            Some(v) => pair(v)
                .filter(|s| s.iter().all(|c| c.is_finite() && *c > 0.0))
                .ok_or_else(|| {
                    RpcError::invalid_params("`size_m` must be [width, height], meters, positive")
                })?,
        };
        let priority = match params.get("priority") {
            None => 0,
            Some(v) => v
                .as_i64()
                .and_then(|p| i32::try_from(p).ok())
                .ok_or_else(|| RpcError::invalid_params("`priority` must be a 32-bit integer"))?,
        };
        let color = match params.get("color") {
            None => [255; 4],
            Some(v) => v
                .as_array()
                .filter(|a| a.len() == 4)
                .and_then(|a| {
                    let bytes: Option<Vec<u8>> = a
                        .iter()
                        .map(|c| c.as_u64().and_then(|b| u8::try_from(b).ok()))
                        .collect();
                    <[u8; 4]>::try_from(bytes?).ok()
                })
                .ok_or_else(|| RpcError::invalid_params("`color` must be [r, g, b, a], 0..=255"))?,
        };
        let panel = regia_xr::XrPanel {
            id,
            priority,
            space,
            pose,
            size_m,
            image: PanelImage::Solid(color),
        };
        if !world.resource_mut::<regia_xr::XrPanels>().set(panel) {
            return Err(xr_error(&XrError::InvalidValue {
                key: "id".into(),
                reason: format!("at most {MAX_PANELS} panels"),
            }));
        }
        let count = world.resource::<regia_xr::XrPanels>().len();
        Ok(json!({ "id": id, "count": count }))
    }
}

/// `xr.set_option { key, value }` (spec 58 § 5.1).
pub(crate) fn xr_set_option(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::set_option(world, params);
    #[cfg(not(feature = "xr"))]
    {
        let _ = (world, params);
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

/// `xr.layers {}`.
pub(crate) fn xr_layers(world: &mut World) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::layers(world);
    #[cfg(not(feature = "xr"))]
    {
        let _ = world;
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

/// `xr.set_quad { id, ... }`.
pub(crate) fn xr_set_quad(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::set_quad(world, params);
    #[cfg(not(feature = "xr"))]
    {
        let _ = (world, params);
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

/// `xr.recenter {}`.
pub(crate) fn xr_recenter(world: &mut World) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::recenter(world);
    #[cfg(not(feature = "xr"))]
    {
        let _ = world;
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

/// `xr.capture_eye { eye, width?, height? }`.
pub(crate) fn xr_capture_eye(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    #[cfg(feature = "xr")]
    return ext::capture_eye(world, params);
    #[cfg(not(feature = "xr"))]
    {
        let _ = (world, params);
        Ok(json!({ "refused": NOT_INSTALLED, "installed": false }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(XrRuntime::null());
        world
    }

    fn drive(world: &mut World, params: Value) -> Result<Value, RpcError> {
        xr_drive(world, &params)
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    #[test]
    fn absent_runtime_is_reported_not_an_error() {
        let mut world = World::new();
        assert_eq!(xr_get(&mut world).expect("get")["installed"], false);
        let got = drive(&mut world, json!({ "action": "ready" })).expect("answered");
        assert_eq!(got["refused"], NOT_INSTALLED);
    }

    #[test]
    fn drive_walks_the_session_and_runs_frames() {
        let mut world = world();
        assert_eq!(xr_get(&mut world).expect("get")["state"], "idle");
        drive(&mut world, json!({ "action": "ready" })).expect("ready");
        drive(&mut world, json!({ "action": "begin" })).expect("begin");
        let got = drive(&mut world, json!({ "action": "frame", "count": 3 })).expect("frames");
        assert_eq!(got["frames"], 3);
        let state = xr_get(&mut world).expect("get");
        assert_eq!(state["state"], "running");
        assert_eq!(state["last_frame"]["frame_index"], 2);
        assert_eq!(state["latch"]["latched"], 3);
        assert_eq!(state["controllers"].as_array().map(Vec::len), Some(2));
        drive(&mut world, json!({ "action": "exit" })).expect("exit");
        drive(&mut world, json!({ "action": "end" })).expect("end");
        let state = xr_get(&mut world).expect("get");
        assert_eq!(state["state"], "idle");
        let kinds: Vec<_> = state["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|e| e["state"].clone())
            .collect();
        assert_eq!(kinds, ["ready", "running", "stopping", "idle"]);
    }

    #[test]
    fn drive_errors_carry_xr_codes() {
        let mut world = world();
        let e = drive(&mut world, json!({ "action": "begin" })).expect_err("idle");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_transition"));
        let e = drive(&mut world, json!({ "action": "frame" })).expect_err("idle");
        assert_eq!(reason(&e).as_deref(), Some("xr_not_running"));
        drive(&mut world, json!({ "action": "ready" })).expect("ready");
        drive(&mut world, json!({ "action": "begin" })).expect("begin");
        let e = drive(
            &mut world,
            json!({ "action": "haptic", "hand": "left", "amplitude": 2.0 }),
        )
        .expect_err("amp");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_haptic"));
    }

    #[test]
    fn drive_rejects_bad_parameters() {
        let mut world = world();
        assert!(drive(&mut world, json!({})).is_err());
        assert!(drive(&mut world, json!({ "action": "nope" })).is_err());
        assert!(drive(&mut world, json!({ "action": "frame", "count": 0 })).is_err());
        assert!(drive(&mut world, json!({ "action": "frame", "count": 1001 })).is_err());
        assert!(drive(&mut world, json!({ "action": "head" })).is_err());
        assert!(drive(&mut world, json!({ "action": "head", "position": [0, 1] })).is_err());
        assert!(drive(&mut world, json!({ "action": "haptic", "hand": "middle" })).is_err());
        // Finite in f64, infinite in f32; a negative or non-number pulse length (ADR-1660).
        for params in [
            json!({ "action": "head", "position": [1e300, 0.0, 0.0] }),
            json!({ "action": "pose", "head": { "position": [1e300, 0.0, 0.0] } }),
            json!({ "action": "pose", "head": { "position": [0.0, 0.0, 0.0], "orientation": [1e300, 0.0, 0.0, 1.0] } }),
            json!({ "action": "head", "position": [0.0, 1.7, 0.0], "yaw": 1e308 }),
            json!({ "action": "head", "position": [0.0, 1.7, 0.0], "yaw": "left" }),
            json!({ "action": "haptic", "hand": "left", "duration_ms": -5.0 }),
            json!({ "action": "haptic", "hand": "left", "duration_ms": "long" }),
        ] {
            let e = drive(&mut world, params.clone()).expect_err("bad");
            assert_eq!(e.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
    }

    #[test]
    fn head_moves_the_next_frame_views() {
        let mut world = world();
        drive(&mut world, json!({ "action": "ready" })).expect("ready");
        drive(&mut world, json!({ "action": "begin" })).expect("begin");
        drive(
            &mut world,
            json!({ "action": "head", "position": [0.0, 1.7, 0.0] }),
        )
        .expect("head");
        drive(&mut world, json!({ "action": "frame" })).expect("frame");
        let state = xr_get(&mut world).expect("get");
        let y = state["last_frame"]["views"][0]["pose"]["position"][1]
            .as_f64()
            .expect("y");
        assert!((y - 1.7).abs() < 1e-4);
    }

    #[test]
    fn session_state_walks_a_bare_backend_without_the_plugin() {
        let mut world = world();
        drive(
            &mut world,
            json!({ "action": "session_state", "state": "focused" }),
        )
        .expect("focused");
        assert_eq!(xr_get(&mut world).expect("get")["state"], "focused");
        drive(
            &mut world,
            json!({ "action": "session_state", "state": "stopping" }),
        )
        .expect("stop");
        assert_eq!(xr_get(&mut world).expect("get")["state"], "stopping");
        drive(
            &mut world,
            json!({ "action": "session_state", "state": "exiting" }),
        )
        .expect("exit");
        assert_eq!(xr_get(&mut world).expect("get")["state"], "exiting");
        let e =
            drive(&mut world, json!({ "action": "session_state", "state": "ready" })).expect_err("terminal");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_transition"));
        assert!(drive(&mut world, json!({ "action": "session_state", "state": "nope" })).is_err());
        assert!(drive(&mut world, json!({ "action": "session_state", "state": "idle" })).is_err());
    }

    #[test]
    fn pose_with_one_view_field_keeps_the_other() {
        let mut world = world();
        drive(
            &mut world,
            json!({ "action": "pose", "ipd_m": 0.07, "fov_deg": 90.0 }),
        )
        .expect("both");
        let (ipd, fov) = world
            .resource::<XrRuntime>()
            .backend()
            .view_config()
            .expect("sim");
        drive(&mut world, json!({ "action": "pose", "ipd_m": 0.06 })).expect("ipd only");
        let (ipd2, fov2) = world
            .resource::<XrRuntime>()
            .backend()
            .view_config()
            .expect("sim");
        assert!((ipd - 0.07).abs() < 1e-6 && (ipd2 - 0.06).abs() < 1e-6);
        assert_eq!(fov, fov2, "fov survives an ipd-only pose");
        drive(&mut world, json!({ "action": "pose", "fov_deg": 70.0 })).expect("fov only");
        let (ipd3, fov3) = world
            .resource::<XrRuntime>()
            .backend()
            .view_config()
            .expect("sim");
        assert!((ipd3 - 0.06).abs() < 1e-6, "ipd survives a fov-only pose");
        assert_ne!(fov3, fov2);
    }

    #[test]
    fn pose_drives_head_controllers_and_view_config() {
        let mut world = world();
        drive(
            &mut world,
            json!({ "action": "session_state", "state": "focused" }),
        )
        .expect("focused");
        drive(
            &mut world,
            json!({ "action": "pose", "head": { "position": [0.0, 1.5, 0.0], "yaw": 0.2 },
                    "right": { "position": [0.2, 1.0, -0.3] }, "ipd_m": 0.07, "fov_deg": 90.0 }),
        )
        .expect("pose");
        drive(&mut world, json!({ "action": "frame" })).expect("frame");
        let got = xr_get(&mut world).expect("get");
        assert_eq!(got["controllers"][1]["active"], true);
        let x0 = got["last_frame"]["views"][0]["pose"]["position"][0]
            .as_f64()
            .expect("x");
        let x1 = got["last_frame"]["views"][1]["pose"]["position"][0]
            .as_f64()
            .expect("x");
        let z0 = got["last_frame"]["views"][0]["pose"]["position"][2]
            .as_f64()
            .expect("z");
        let z1 = got["last_frame"]["views"][1]["pose"]["position"][2]
            .as_f64()
            .expect("z");
        assert!((((x1 - x0).powi(2) + (z1 - z0).powi(2)).sqrt() - 0.07).abs() < 1e-5);
        let e = drive(&mut world, json!({ "action": "pose", "ipd_m": 0.5 })).expect_err("ipd");
        assert_eq!(reason(&e).as_deref(), Some("xr_invalid_ipd"));
        assert!(
            drive(
                &mut world,
                json!({ "action": "pose", "head": { "position": [0, 1] } })
            )
            .is_err()
        );
    }

    #[cfg(feature = "xr")]
    mod plugin {
        use super::*;
        use regia_xr::{XrBackendKind, XrConfig, XrPlugin};

        fn app() -> App {
            let mut app = App::new();
            enable_fixed_step(&mut app, 60.0);
            app.add_plugins(XrPlugin::new(XrConfig {
                backend: XrBackendKind::Simulator,
                ..XrConfig::default()
            }));
            app
        }

        #[test]
        fn get_reports_the_app_state_and_options() {
            let mut app = app();
            let got = xr_get(app.world_mut()).expect("get");
            assert_eq!(got["installed"], true);
            assert_eq!(got["app_state"], "Idle");
            assert_eq!(got["backend_kind"], "simulator");
            assert_eq!(got["actual_tracking_space"], "stage");
            assert_eq!(got["hand_tracking"], "unsupported");
            assert_eq!(got["stats"]["stale_frames"], 0);
            drive(
                app.world_mut(),
                json!({ "action": "session_state", "state": "focused" }),
            )
            .expect("focused");
            assert_eq!(xr_get(app.world_mut()).expect("get")["app_state"], "Focused");
        }

        #[test]
        fn input_reaches_the_actions_only_when_focused() {
            let mut app = app();
            drive(
                app.world_mut(),
                json!({ "action": "session_state", "state": "visible" }),
            )
            .expect("visible");
            drive(
                app.world_mut(),
                json!({ "action": "input", "name": "primary", "sub": "right", "value": true }),
            )
            .expect("input");
            app.update();
            let got = xr_get(app.world_mut()).expect("get");
            let primary = |g: &Value| {
                g["actions"].as_array().and_then(|a| {
                    a.iter()
                        .find(|x| x["name"] == "primary" && x["sub"] == "right")
                        .cloned()
                })
            };
            assert_eq!(primary(&got).map(|a| a["active"].clone()), Some(json!(false)));
            drive(
                app.world_mut(),
                json!({ "action": "session_state", "state": "focused" }),
            )
            .expect("focused");
            app.update();
            let got = xr_get(app.world_mut()).expect("get");
            assert_eq!(primary(&got).map(|a| a["value"].clone()), Some(json!(true)));
            let e = drive(
                app.world_mut(),
                json!({ "action": "input", "name": "warp", "value": 1.0 }),
            )
            .expect_err("unknown");
            assert_eq!(reason(&e).as_deref(), Some("xr_action_unknown"));
        }

        #[test]
        fn set_option_round_trips_and_get_reflects_it() {
            let mut app = app();
            let w = app.world_mut();
            let got = xr_set_option(w, &json!({ "key": "render_scale", "value": 1.25 })).expect("set");
            assert_eq!(got["old"], 1.0);
            assert_eq!(got["new"], 1.25);
            assert_eq!(got["applied_at"], "next_frame");
            assert_eq!(xr_get(w).expect("get")["render_scale"], 1.25);
            let got = xr_set_option(w, &json!({ "key": "tracking_space", "value": "local" })).expect("set");
            assert_eq!(got["applied_at"], "now");
            assert_eq!(xr_get(w).expect("get")["actual_tracking_space"], "local");
            let e = xr_set_option(w, &json!({ "key": "warp", "value": 1 })).expect_err("key");
            assert_eq!(reason(&e).as_deref(), Some("xr_unknown_option"));
            let e = xr_set_option(w, &json!({ "key": "render_scale", "value": 9 })).expect_err("range");
            assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
            let e = xr_set_option(w, &json!({ "key": "mirror", "value": [1] })).expect_err("type");
            assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
            let got = xr_set_option(w, &json!({ "key": "enabled", "value": false })).expect("off");
            assert_eq!(got["new"], false);
            assert_eq!(xr_get(w).expect("get")["app_state"], "Off");
            assert!(xr_set_option(w, &json!({ "value": 1 })).is_err());
        }

        /// Spec 58 § 10 through the dispatcher: scripted frame times over budget raise
        /// `xr_over_budget` (`stats.over_budget`, `budget.last`), and `auto_scale` lowers the
        /// render scale one step per raise.
        #[test]
        fn dispatched_over_budget_frames_raise_the_diagnostic_and_auto_scale_steps_down() {
            fn call(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
                crate::dispatch::call_in_process(app.world_mut(), method, params)
                    .expect("xr methods answer synchronously")
            }
            let mut app = app();
            call(
                &mut app,
                "xr.drive",
                json!({ "action": "session_state", "state": "focused" }),
            )
            .expect("focused");
            let got = call(
                &mut app,
                "xr.set_option",
                json!({ "key": "auto_scale", "value": true }),
            )
            .expect("auto_scale is an option");
            assert_eq!(
                (got["old"].clone(), got["new"].clone()),
                (json!(false), json!(true))
            );
            assert_eq!(got["applied_at"], "now");
            let got = call(&mut app, "xr.get", json!({})).expect("get");
            let budget_ms = got["budget"]["budget_ms"].as_f64().expect("budget_ms");
            let frames = got["budget"]["frames"].as_u64().expect("frames");
            assert_eq!(got["budget"]["auto_scale"], true);
            assert_eq!(got["stats"]["over_budget"], 0);
            call(
                &mut app,
                "xr.drive",
                json!({ "action": "frame_time", "ms": budget_ms * 2.0 }),
            )
            .expect("scripted frame time");
            for _ in 0..frames - 1 {
                app.update();
            }
            let got = call(&mut app, "xr.get", json!({})).expect("get");
            assert_eq!(got["stats"]["over_budget"], 0);
            assert_eq!(got["budget"]["over_run"], frames - 1);
            app.update();
            let got = call(&mut app, "xr.get", json!({})).expect("get");
            assert_eq!(got["stats"]["over_budget"], 1);
            assert_eq!(
                got["render_scale"].as_f64().map(|s| (s * 100.0).round()),
                Some(90.0)
            );
            assert_eq!(got["budget"]["last"]["code"], "xr_over_budget");
            assert_eq!(got["budget"]["last"]["scale_from"], 1.0);
            assert_eq!(got["budget"]["over_run"], 0);
            let e = call(
                &mut app,
                "xr.drive",
                json!({ "action": "frame_time", "ms": -1.0 }),
            )
            .expect_err("negative frame time");
            assert_eq!(e.code, crate::protocol::INVALID_PARAMS);
            call(
                &mut app,
                "xr.drive",
                json!({ "action": "frame_time", "ms": null }),
            )
            .expect("cleared");
            let got = call(&mut app, "xr.get", json!({})).expect("get");
            assert_eq!(got["budget"]["scripted_ms"], Value::Null);
        }

        #[test]
        fn recenter_and_capture_eye() {
            let mut app = app();
            let e = xr_capture_eye(app.world_mut(), &json!({ "eye": "both" })).expect_err("idle");
            assert_eq!(reason(&e).as_deref(), Some("xr_not_running"));
            drive(
                app.world_mut(),
                json!({ "action": "session_state", "state": "focused" }),
            )
            .expect("focused");
            drive(
                app.world_mut(),
                json!({ "action": "pose", "head": { "position": [0.4, 1.6, -1.0], "yaw": 0.5 } }),
            )
            .expect("pose");
            app.update();
            let got = xr_recenter(app.world_mut()).expect("recenter");
            assert!(got["offset"]["position"].is_array());
            let got = xr_capture_eye(app.world_mut(), &json!({ "eye": "both" })).expect("capture");
            let eyes = got["eyes"].as_array().expect("eyes");
            assert_eq!(eyes.len(), 2);
            assert_eq!(got["image_source"], "none");
            assert_eq!(eyes[0]["view_projection"].as_array().map(Vec::len), Some(16));
            let one = xr_capture_eye(app.world_mut(), &json!({ "eye": 1 })).expect("capture");
            assert_eq!(one["eyes"][0]["eye"], 1);
            assert!(xr_capture_eye(app.world_mut(), &json!({ "eye": 2 })).is_err());
            assert!(xr_capture_eye(app.world_mut(), &json!({ "eye": 0, "width": 0 })).is_err());
        }

        #[test]
        fn the_render_surface_without_a_renderer_stays_quiet() {
            let mut app = app();
            let got = xr_layers(app.world_mut()).expect("layers");
            assert_eq!(got["count"], 0);
            assert_eq!(got["max_layers"], 16);
            assert_eq!(got["depth_layer_supported"], true);
            let e =
                xr_capture_eye(app.world_mut(), &json!({ "eye": 0, "source": "nope" })).expect_err("source");
            assert!(e.message.contains("source"), "{}", e.message);
            assert!(xr_get(app.world_mut()).expect("get").get("render").is_none());
            let refused = xr_set_quad(app.world_mut(), &json!({ "id": 1 })).expect("no panels resource");
            assert_eq!(refused["refused"], NOT_INSTALLED);
        }

        #[test]
        fn quads_set_replace_and_remove_with_named_errors() {
            let mut app = app();
            let w = app.world_mut();
            w.init_resource::<regia_xr::XrPanels>();
            assert!(xr_set_quad(w, &json!({})).is_err(), "id required");
            assert!(xr_set_quad(w, &json!({ "id": 1, "space": "moon" })).is_err());
            assert!(xr_set_quad(w, &json!({ "id": 1, "position": "here" })).is_err());
            assert!(xr_set_quad(w, &json!({ "id": 1, "size_m": [1.0] })).is_err());
            assert!(xr_set_quad(w, &json!({ "id": 1, "size_m": [0.0, 1.0] })).is_err());
            assert!(xr_set_quad(w, &json!({ "id": 1, "color": [1, 2, 3, 999] })).is_err());
            let got = xr_set_quad(
                w,
                &json!({ "id": 1, "space": "head", "position": [0, 0, -1], "yaw": 0.2,
                         "size_m": [0.5, 0.5], "priority": 3, "color": [255, 0, 0, 255] }),
            )
            .expect("set");
            assert_eq!(got["count"], 1);
            let panels = w.resource::<regia_xr::XrPanels>();
            let panel = panels.get(1).expect("panel");
            assert_eq!(panel.priority, 3);
            assert_eq!(panel.space, regia_platform::xr::LayerSpace::Head);
            assert!((panel.pose.position.z + 1.0).abs() < 1e-6);
            assert_eq!(panel.size_m, [0.5, 0.5]);
            xr_set_quad(w, &json!({ "id": 1, "priority": 4 })).expect("replace");
            let panels = w.resource::<regia_xr::XrPanels>();
            assert_eq!(panels.len(), 1);
            assert!(
                (panels.get(1).expect("panel").pose.position.z + 1.0).abs() < 1e-6,
                "the pose stays"
            );
            assert_eq!(
                xr_set_quad(w, &json!({ "id": 1, "remove": true })).expect("rm")["removed"],
                true
            );
            assert_eq!(
                xr_set_quad(w, &json!({ "id": 1, "remove": true })).expect("rm")["removed"],
                false
            );
        }

        #[test]
        fn lens_and_mono_drive_the_simulator() {
            let mut app = app();
            let w = app.world_mut();
            drive(w, json!({ "action": "session_state", "state": "focused" })).expect("focused");
            drive(w, json!({ "action": "lens", "radius": 0.9 })).expect("lens");
            let rt = w.resource::<XrRuntime>();
            assert!(
                rt.backend()
                    .visibility_mask(regia_platform::xr::Eye::Left)
                    .is_some()
            );
            let e = drive(w, json!({ "action": "lens", "radius": 9.0 })).expect_err("radius");
            assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
            assert!(drive(w, json!({ "action": "lens" })).is_err());
            drive(w, json!({ "action": "mono", "on": true })).expect("mono");
            assert!(drive(w, json!({ "action": "mono" })).is_err());
            let options = [
                ("stereo_mode", json!("multiview")),
                ("hidden_area", json!(false)),
                ("depth_layer", json!(false)),
            ];
            for (key, value) in options {
                let got = xr_set_option(w, &json!({ "key": key, "value": value })).expect("option");
                assert_eq!(got["new"], value, "{key}");
                assert_eq!(got["applied_at"], "next_frame");
            }
            let e = xr_set_option(w, &json!({ "key": "stereo_mode", "value": "triple" })).expect_err("mode");
            assert_eq!(reason(&e).as_deref(), Some("xr_invalid_value"));
        }

        /// The whole surface through the renderer, on any headless adapter. Skipped, with a note,
        /// when none exists.
        #[test]
        fn an_agent_sees_the_eyes_the_layers_and_the_mirror() {
            let Ok(device) = crate::seeing::headless_render_device() else {
                eprintln!("skipped: no GPU adapter");
                return;
            };
            let mut app = App::new();
            enable_fixed_step(&mut app, 60.0);
            app.add_plugins(regia_app::MinimalPlugins);
            app.add_plugins(regia_scene::ScenePlugin);
            app.init_resource::<regia_input::InputInjection>();
            app.insert_resource(device.clone());
            app.add_plugins(regia_render::RenderPlugin { headless: true });
            app.init_resource::<regia_render::RuntimeRenderer>();
            app.add_plugins(regia_xr::XrPlugin::simulator());
            app.add_plugins(regia_xr::XrRenderPlugin);
            app.world_mut()
                .resource_mut::<regia_xr::XrRenderProfile>()
                .recommended_extent = regia_core::glam::UVec2::new(32, 32);
            let quad = regia_render::LabMesh {
                vertices: [[-9.0, -9.0], [9.0, -9.0], [9.0, 9.0], [-9.0, 9.0]]
                    .into_iter()
                    .map(|[x, y]| {
                        regia_render::LabVertex::from_pos_normal_uv([x, y, -4.0], [0.0, 0.0, 1.0], [0.0, 0.0])
                    })
                    .collect(),
                indices: vec![0, 1, 2, 0, 2, 3],
                ..regia_render::LabMesh::unskinned()
            };
            let lab = regia_render::LabScene {
                width: 32,
                height: 32,
                eye: [0.0, 0.0, 0.0],
                target: [0.0, 0.0, -1.0],
                fov_y: 1.0,
                near: 0.05,
                far: 50.0,
                depth_prepass: true,
                directional_shadows: false,
                ibl: false,
                exposure_ev: 12.0,
                ambient_lux: 20_000.0,
                double_sided: true,
                post: None,
                directional: regia_render::DirectionalLightUniform::new([0.0, 0.0, -1.0], [1.0; 3], 10_000.0),
                local_lights: Vec::new(),
                materials: vec![regia_render::LabMaterial::default()],
                meshes: vec![quad],
            };
            regia_render::spawn_lab_scene(app.world_mut(), &lab);
            let w = app.world_mut();
            drive(w, json!({ "action": "session_state", "state": "focused" })).expect("focused");
            drive(w, json!({ "action": "lens", "radius": 0.9 })).expect("lens");
            xr_set_quad(
                w,
                &json!({ "id": 2, "space": "head", "position": [0, 0, -1], "color": [255, 0, 0, 255] }),
            )
            .expect("quad");
            app.update();
            app.update();
            let w = app.world_mut();
            let got = xr_get(w).expect("get");
            assert_eq!(got["render"]["mode"], "two_pass");
            assert_eq!(got["render"]["scene_passes"], 2);
            assert_eq!(got["render"]["quads_composed"], 1);
            assert!(got["render"]["hidden_triangles"][0].as_u64() > Some(0));
            assert_eq!(got["render"]["depth_layers"], 2);
            assert!(
                got["render"]["disabled_effects"]
                    .as_array()
                    .is_some_and(|d| !d.is_empty())
            );
            let layers = xr_layers(w).expect("layers");
            assert_eq!(layers["count"], 2);
            assert_eq!(layers["quads"][0]["id"], 2);
            assert!(layers["projection"]["views"][1]["depth"]["far_m"].is_number());
            let eyes = xr_capture_eye(w, &json!({ "eye": "both" })).expect("eyes");
            assert_eq!(eyes["image_source"], "eye_targets");
            assert_eq!(eyes["eyes"][1]["image"]["width"], 32);
            let shown = xr_capture_eye(
                w,
                &json!({ "eye": 0, "source": "display", "width": 16, "height": 16 }),
            )
            .expect("display");
            assert_eq!(shown["image_source"], "display");
            assert_eq!(shown["eyes"][0]["image"]["width"], 16);
            let mirror = xr_capture_eye(
                w,
                &json!({ "eye": 0, "source": "mirror", "width": 64, "height": 32 }),
            )
            .expect("mirror");
            assert_eq!(mirror["image_source"], "mirror");
            assert_eq!(mirror["mirror"]["height"], 32);
            assert!(mirror["mirror"]["base64"].as_str().is_some_and(|b| !b.is_empty()));
        }

        #[test]
        fn a_real_backend_refuses_simulator_drives() {
            let mut world = World::new();
            struct Real(regia_platform::xr::NullXr);
            impl regia_platform::xr::XrBackend for Real {
                fn name(&self) -> &'static str {
                    "openxr"
                }
                fn state(&self) -> SessionState {
                    self.0.state()
                }
                fn poll_events(&mut self) -> Vec<XrEvent> {
                    self.0.poll_events()
                }
                fn begin_session(&mut self) -> Result<(), XrError> {
                    self.0.begin_session()
                }
                fn request_exit(&mut self) -> Result<(), XrError> {
                    self.0.request_exit()
                }
                fn end_session(&mut self) -> Result<(), XrError> {
                    self.0.end_session()
                }
                fn wait_frame(&mut self) -> Result<regia_platform::xr::FrameTiming, XrError> {
                    self.0.wait_frame()
                }
                fn begin_frame(&mut self) -> Result<(), XrError> {
                    self.0.begin_frame()
                }
                fn locate_views(&mut self, t: i64) -> Result<[regia_platform::xr::View; 2], XrError> {
                    self.0.locate_views(t)
                }
                fn head_sample(&self) -> regia_platform::xr::PoseSample {
                    self.0.head_sample()
                }
                fn sync_actions(&mut self) -> Result<(), XrError> {
                    self.0.sync_actions()
                }
                fn controller(&self, hand: Hand) -> regia_platform::xr::ControllerState {
                    self.0.controller(hand)
                }
                fn apply_haptic(&mut self, h: Hand, a: f32, d: i64) -> Result<(), XrError> {
                    self.0.apply_haptic(h, a, d)
                }
                fn end_frame(&mut self, s: &regia_platform::xr::FrameSubmit) -> Result<(), XrError> {
                    self.0.end_frame(s)
                }
            }
            world.insert_resource(XrRuntime::new(Box::new(Real(Default::default()))));
            for params in [
                json!({ "action": "session_state", "state": "ready" }),
                json!({ "action": "pose", "head": { "position": [0, 1, 0] } }),
                json!({ "action": "input", "name": "menu", "sub": "left", "value": true }),
            ] {
                let e = drive(&mut world, params).expect_err("real backend");
                assert_eq!(reason(&e).as_deref(), Some("xr_simulator_only"));
            }
        }
    }
}
