// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for demo recording and the spectator camera (`book/agents/demo.md`,
//! ADR-1569, ADR-1878): `demo.inspect`, `demo.seek_plan`, `demo.status`, `demo.record_start`,
//! `demo.record_stop`, `demo.play`, `demo.pause`, `demo.seek`, `demo.scrub_bar`,
//! `spectator.get`, `spectator.set`, `spectator.step`, `spectator.bind`.
//!
//! `demo.inspect` and `demo.seek_plan` read a demo file ([`DemoRecording`], `.regiademo` or
//! `.frec`) and never run a simulation. The live methods drive the world's
//! [`DemoDriver`] ([`regia_framework::demo_world`]): requests are queued and applied in `Last` of
//! the same frame, so a caller reads the outcome with `demo.status` (or `wait_until` on it) after
//! the frame. The `spectator.*` methods drive the world's [`SpectatorCamera`] resource (created on
//! first use), so an agent can fly or follow headless and read the pose back; `spectator.bind`
//! puts the pose on a render camera.

use std::path::PathBuf;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_framework::demo::{DemoError, DemoHeader, DemoRecording};
use regia_framework::demo_scrub::DemoScrubBar;
use regia_framework::demo_world::{DemoDriver, DemoMode, DemoRequest, demo_status};
use regia_framework::spectator::{
    SpectatorCamera, SpectatorConfig, SpectatorDrive, SpectatorInput, SpectatorMode, SpectatorOverheadConfig,
    SpectatorView,
};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Keyframes `demo.inspect` lists before it says `keyframes_truncated`.
const KEYFRAME_LIST_CAP: usize = 200;
/// Frames one `spectator.step` integrates.
const STEP_FRAMES_MAX: u64 = 100_000;

fn demo_error(method: &str, error: &DemoError) -> RpcError {
    RpcError::new(
        protocol::INVALID_PARAMS,
        &format!("demo_{}", error.code()),
        format!("{method}: {error}"),
        "the file must be a complete .regiademo (format_version 1 or 2, ends with the `// regia-demo-end` line) or .frec match record",
    )
}

/// Keyframe ticks `demo.status` lists before it says `keyframes_truncated`.
const STATUS_KEYFRAME_CAP: usize = 200;

fn live_error(code: &str, message: impl Into<String>, hint: &str) -> RpcError {
    RpcError::new(protocol::INVALID_PARAMS, code, message, hint)
}

fn not_installed(method: &str) -> RpcError {
    RpcError::new(
        protocol::NOT_FOUND,
        "demo_not_installed",
        format!("{method}: no DemoDriver in this world"),
        "add regia_framework::demo_world::DemoWorldPlugin to the app (and DemoScrubPlugin for the scrub bar)",
    )
}

fn path_param(params: &Value) -> Result<&str, RpcError> {
    params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (the .regiademo file) is required"))
}

/// The demo `path`, confined to the same roots as `render.out_start` (project root, system temp
/// dir, `output_roots`; no `..`, no symlink out): a read of any host file would echo
/// its length in the truncation error and could block the world thread on a huge file.
fn demo_path(world: &World, params: &Value) -> Result<std::path::PathBuf, RpcError> {
    crate::render_out::confine_path(world, "path", std::path::Path::new(path_param(params)?))
}

fn tick_param(params: &Value) -> Result<u64, RpcError> {
    params
        .get("tick")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`tick` must be a non-negative integer"))
}

/// `demo.inspect`: the header, counts and keyframe index of a demo file.
pub fn demo_inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = path_param(params)?;
    let demo =
        DemoRecording::load_any(&demo_path(world, params)?).map_err(|e| demo_error("demo.inspect", &e))?;
    let h = &demo.header;
    let mismatches = h.mismatches(&regia_framework::demo::DemoHeader::new(
        h.level.clone(),
        h.seed,
        h.tick_rate_hz,
        h.keyframe_interval,
    ));
    Ok(json!({
        "path": path,
        "header": {
            "format_version": h.format_version,
            "engine_version": h.engine_version,
            "build_hash": h.build_hash,
            "content_hash": h.content_hash,
            "seed": h.seed.to_string(),
            "level": h.level,
            "tick_rate_hz": h.tick_rate_hz,
            "step_count": h.step_count,
            "keyframe_interval": h.keyframe_interval,
        },
        "engine_matches": mismatches.is_empty(),
        "mismatches": mismatches,
        // Recorded in a session that reloaded extension code (ADR-9850 (b)).
        "nondeterministic": h.nondeterministic(),
        "actions": demo.actions,
        "steps": demo.len(),
        "duration_secs": demo.duration_secs(),
        "keyframe_count": demo.keyframes.len(),
        "keyframes": demo.keyframes.iter().take(KEYFRAME_LIST_CAP).map(|k| json!({
            "tick": k.tick,
            "state_hash": format!("{:016x}", k.state_hash),
            "snapshot_bytes": k.snapshot_hex.len() / 2,
        })).collect::<Vec<_>>(),
        "keyframes_truncated": demo.keyframes.len() > KEYFRAME_LIST_CAP,
        "has_timestamps": !demo.timestamps.is_empty(),
    }))
}

/// `demo.seek_plan`: which keyframe a seek to `tick` would restore and how many steps it would
/// replay.
pub fn demo_seek_plan(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tick = tick_param(params)?;
    let demo =
        DemoRecording::load_any(&demo_path(world, params)?).map_err(|e| demo_error("demo.seek_plan", &e))?;
    if tick > demo.len() {
        return Err(demo_error(
            "demo.seek_plan",
            &DemoError::TickOutOfRange {
                tick,
                steps: demo.len(),
            },
        ));
    }
    let key = demo
        .nearest_keyframe(tick)
        .ok_or_else(|| demo_error("demo.seek_plan", &DemoError::NoKeyframe { tick }))?;
    Ok(json!({
        "tick": tick,
        "keyframe_tick": key.tick,
        "steps_to_replay": tick - key.tick,
        "state_hash": format!("{:016x}", key.state_hash),
        "exact_keyframe": key.tick == tick,
    }))
}

fn status_json(world: &World) -> Option<Value> {
    let status = demo_status(world)?;
    let driver = world.get_resource::<DemoDriver>()?;
    let keyframes: Vec<u64> = status
        .keyframe_ticks
        .iter()
        .take(STATUS_KEYFRAME_CAP)
        .copied()
        .collect();
    Some(json!({
        "mode": status.mode.name(),
        "tick": status.tick,
        "steps": status.steps,
        "tick_rate_hz": status.tick_rate_hz,
        "keyframes": keyframes,
        "keyframes_truncated": status.keyframe_ticks.len() > STATUS_KEYFRAME_CAP,
        "pending": driver.pending(),
        "diverged_at": driver.diverged_at(),
        "last_error": driver
            .last_error()
            .map(|(code, message)| json!({ "code": format!("demo_{code}"), "message": message })),
        "last_seek": driver.last_seek().map(|seek| json!({
            "tick": seek.tick,
            "keyframe_tick": seek.keyframe_tick,
            "steps_replayed": seek.steps_replayed,
            "keyframes_checked": seek.keyframes_checked,
        })),
        // Project-relative, as the request named it (the write itself is under the project root).
        "saved_to": driver.saved_to().map(|p| {
            let root = crate::req_path::project_root(world);
            p.strip_prefix(&root).unwrap_or(p).to_string_lossy().into_owned()
        }),
        "scrub_bar_open": world.get_resource::<DemoScrubBar>().is_some_and(|bar| bar.open),
    }))
}

/// `demo.status`: the live driver's mode, playhead, keyframes and last outcome.
pub fn demo_status_method(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    status_json(world).ok_or_else(|| not_installed("demo.status"))
}

fn queue(world: &mut World, method: &str, request: DemoRequest) -> Result<Value, RpcError> {
    let mut driver = world
        .get_resource_mut::<DemoDriver>()
        .ok_or_else(|| not_installed(method))?;
    driver.request(request);
    let pending = driver.pending();
    Ok(json!({ "queued": true, "pending": pending }))
}

fn opt_str<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a string"))),
    }
}

fn opt_u64(params: &Value, name: &str, max: u64) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| *n <= max)
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be an integer 0..={max}"))),
    }
}

/// A path an agent may write a demo to: relative, only plain components (no `..`, no root, no
/// drive prefix), ending in `.regiademo` or `.frec`.
fn save_path(raw: &str) -> Result<PathBuf, RpcError> {
    let clean = regia_core::project_path::check_relative(raw).ok();
    let checked = clean.filter(|path| {
        path.extension().is_some_and(|ext| {
            ext == regia_framework::demo::DEMO_EXTENSION
                || ext == regia_framework::input_recording::FREC_EXTENSION
        })
    });
    if let Some(path) = checked {
        Ok(path)
    } else {
        Err(live_error(
            "demo_bad_path",
            format!("demo.record_stop: {raw:?} is not a relative .regiademo or .frec path"),
            "use a project-relative path such as saved/demos/run-001.frec (no `..`, no absolute path)",
        ))
    }
}

/// `demo.record_start`: starts recording the running game through the live driver.
pub fn demo_record_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let level = opt_str(params, "level")?.unwrap_or("").to_string();
    let seed = opt_u64(params, "seed", u64::MAX)?.unwrap_or(0);
    let rate = opt_u64(params, "tick_rate_hz", 10_000)?.unwrap_or(60);
    let interval = opt_u64(params, "keyframe_interval", u64::from(u32::MAX))?.unwrap_or(60);
    if rate == 0 {
        return Err(RpcError::invalid_params("`tick_rate_hz` must be at least 1"));
    }
    let actions = match params.get("actions") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()
                .ok_or_else(|| RpcError::invalid_params("`actions` must be an array of action names"))?,
        ),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`actions` must be an array of action names",
            ));
        }
    };
    let driver = world
        .get_resource::<DemoDriver>()
        .ok_or_else(|| not_installed("demo.record_start"))?;
    if driver.mode() == DemoMode::Recording {
        return Err(live_error(
            "demo_busy",
            "demo.record_start: already recording",
            "demo.record_stop first",
        ));
    }
    let header = DemoHeader::new(
        level,
        seed,
        u32::try_from(rate).unwrap_or(60),
        u32::try_from(interval).unwrap_or(u32::MAX),
    )
    .with_build(opt_str(params, "build_hash")?.unwrap_or(""))
    .with_content(opt_str(params, "content_hash")?.unwrap_or(""));
    queue(
        world,
        "demo.record_start",
        DemoRequest::Record { header, actions },
    )
}

/// `demo.record_stop`: stops recording; with `path`, saves the demo (`.frec` or `.regiademo`).
pub fn demo_record_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = opt_str(params, "path")?.map(save_path).transpose()?;
    // The save path is project-relative: resolve it under the project root, not the working
    // directory the framework would write against, and refuse a symlinked folder that leads out
    // (ADR-5869). `demo.play` confines its path the same way.
    let path = path
        .map(|rel| crate::req_path::project_path(world, "path", &rel.to_string_lossy()))
        .transpose()?;
    let driver = world
        .get_resource::<DemoDriver>()
        .ok_or_else(|| not_installed("demo.record_stop"))?;
    if driver.mode() != DemoMode::Recording {
        return Err(live_error(
            "demo_no_demo",
            "demo.record_stop: not recording",
            "demo.record_start first",
        ));
    }
    queue(world, "demo.record_stop", DemoRequest::StopRecording { path })
}

/// `demo.play`: loads a demo file (or replays the last recording) and plays it from tick 0.
pub fn demo_play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let driver = world
        .get_resource::<DemoDriver>()
        .ok_or_else(|| not_installed("demo.play"))?;
    if driver.mode() == DemoMode::Recording {
        return Err(live_error(
            "demo_busy",
            "demo.play: recording",
            "demo.record_stop first",
        ));
    }
    let demo = match opt_str(params, "path")? {
        Some(_) => {
            DemoRecording::load_any(&demo_path(world, params)?).map_err(|e| demo_error("demo.play", &e))?
        }
        None => driver.demo().cloned().ok_or_else(|| {
            live_error(
                "demo_no_demo",
                "demo.play: no path and nothing recorded",
                "pass `path`, or record first",
            )
        })?,
    };
    let steps = demo.len();
    let mut out = queue(world, "demo.play", DemoRequest::Play(Box::new(demo)))?;
    out["steps"] = json!(steps);
    Ok(out)
}

fn loaded_demo<'w>(world: &'w World, method: &str) -> Result<&'w DemoRecording, RpcError> {
    let driver = world
        .get_resource::<DemoDriver>()
        .ok_or_else(|| not_installed(method))?;
    if driver.mode() == DemoMode::Recording {
        return Err(live_error(
            "demo_busy",
            format!("{method}: recording"),
            "demo.record_stop first",
        ));
    }
    match (driver.mode(), driver.demo()) {
        (DemoMode::Idle, _) | (_, None) => Err(live_error(
            "demo_no_demo",
            format!("{method}: no demo is loaded"),
            "demo.play first",
        )),
        (_, Some(demo)) => Ok(demo),
    }
}

/// `demo.pause`: pauses (`paused: true`, the default) or resumes the loaded demo.
pub fn demo_pause(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let paused = match params.get("paused") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`paused` must be a boolean"))?,
    };
    loaded_demo(world, "demo.pause")?;
    queue(world, "demo.pause", DemoRequest::Pause(paused))
}

/// `demo.seek`: puts the world at `tick` of the loaded demo (restores the nearest snapshot
/// keyframe and replays to the tick; applied this frame, read the outcome with `demo.status`).
pub fn demo_seek(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tick = tick_param(params)?;
    let demo = loaded_demo(world, "demo.seek")?;
    if tick > demo.len() {
        return Err(demo_error(
            "demo.seek",
            &DemoError::TickOutOfRange {
                tick,
                steps: demo.len(),
            },
        ));
    }
    let key = demo
        .nearest_keyframe(tick)
        .map(|key| key.tick)
        .ok_or_else(|| demo_error("demo.seek", &DemoError::NoKeyframe { tick }))?;
    let mut out = queue(world, "demo.seek", DemoRequest::Seek(tick))?;
    out["tick"] = json!(tick);
    out["keyframe_tick"] = json!(key);
    out["steps_to_replay"] = json!(tick - key);
    Ok(out)
}

/// `demo.scrub_bar`: shows (`open: true`, the default) or hides the runtime scrub bar.
pub fn demo_scrub_bar(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let open = match params.get("open") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`open` must be a boolean"))?,
    };
    let mut bar = world.get_resource_mut::<DemoScrubBar>().ok_or_else(|| {
        RpcError::new(
            protocol::NOT_FOUND,
            "demo_not_installed",
            "demo.scrub_bar: no DemoScrubBar in this world",
            "add regia_framework::demo_scrub::DemoScrubPlugin (with DemoWorldPlugin and the runtime UI)",
        )
    })?;
    bar.open = open;
    Ok(json!({ "open": open }))
}

fn camera_json(cam: &SpectatorCamera) -> Value {
    let (position, rotation) = cam.pose();
    json!({
        "mode": cam.mode().name(),
        "position": [position.x, position.y, position.z],
        "rotation": [rotation.x, rotation.y, rotation.z, rotation.w],
        "yaw": cam.yaw(),
        "pitch": cam.pitch(),
        "speed_level": cam.speed_level(),
        "speed": cam.speed(),
        "follow_distance": cam.follow_distance(),
        "anchor": [cam.anchor().x, cam.anchor().y, cam.anchor().z],
        "target": cam.target().map(|t| json!([t.x, t.y, t.z])),
        "target_rotation": cam.target_rotation().to_array(),
        "pov_offset": cam.pov_offset().to_array(),
        "overhead_focus": cam.overhead_focus().to_array(),
        "overhead_height": cam.overhead_height(),
        "overhead": {
            "height": cam.overhead_config().height,
            "min_height": cam.overhead_config().min_height,
            "max_height": cam.overhead_config().max_height,
            "pan_speed": cam.overhead_config().pan_speed,
        },
    })
}

fn num(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| n as f32)
            .filter(|n| n.is_finite())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

fn int(params: &Value, name: &str) -> Result<Option<i32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .and_then(|n| i32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a 32-bit integer"))),
    }
}

/// `look` is `[yaw, pitch]` in radians. A third component is ignored. Other lengths are refused.
fn look_xy(params: &Value) -> Result<Option<Vec3>, RpcError> {
    match params.get("look") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let bad = || {
                RpcError::invalid_params(
                    "`look` must be [yaw, pitch] or [yaw, pitch, ignored] of finite numbers",
                )
            };
            let items = v
                .as_array()
                .filter(|a| a.len() == 2 || a.len() == 3)
                .ok_or_else(bad)?;
            let component = |index: usize| {
                items[index]
                    .as_f64()
                    .map(|n| n as f32)
                    .filter(|n| n.is_finite())
                    .ok_or_else(bad)
            };
            Ok(Some(Vec3::new(component(0)?, component(1)?, 0.0)))
        }
    }
}

fn vec3(params: &Value, name: &str) -> Result<Option<Vec3>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let bad = || RpcError::invalid_params(format!("`{name}` must be [x, y, z] of finite numbers"));
            let items = v.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
            let mut out = [0.0f32; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                // Finite as `f32`: a finite `f64` past `f32::MAX` would become infinite.
                *slot = item
                    .as_f64()
                    .map(|n| n as f32)
                    .filter(|n| n.is_finite())
                    .ok_or_else(bad)?;
            }
            Ok(Some(Vec3::from_array(out)))
        }
    }
}

fn camera_mut(world: &mut World) -> impl std::ops::DerefMut<Target = SpectatorCamera> + '_ {
    if world.get_resource::<SpectatorCamera>().is_none() {
        world.insert_resource(SpectatorCamera::new(SpectatorConfig::default(), Vec3::ZERO));
    }
    world.resource_mut::<SpectatorCamera>()
}

/// `spectator.get`: the camera's pose and state (creating the default camera if absent).
pub fn spectator_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(camera_json(&camera_mut(world)))
}

/// `spectator.set`: mode, pose, speed level and follow target. Every parameter is optional and
/// validated before mutation. Target changes precede mode selection, so Overhead starts at a
/// target provided in the same request; an explicit overhead focus applies last.
pub fn spectator_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let position = vec3(params, "position")?;
    let target = vec3(params, "target")?;
    let yaw = num(params, "yaw")?;
    let pitch = num(params, "pitch")?;
    let level = int(params, "speed_level")?;
    let pov_offset = vec3(params, "pov_offset")?;
    let overhead_focus = vec3(params, "overhead_focus")?;
    let target_rotation = match params.get("target_rotation") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let bad = || {
                RpcError::invalid_params("`target_rotation` must be a nonzero finite [x, y, z, w] quaternion")
            };
            let values = value.as_array().filter(|v| v.len() == 4).ok_or_else(bad)?;
            let mut components = [0.0_f32; 4];
            for (component, value) in components.iter_mut().zip(values) {
                *component = value
                    .as_f64()
                    .map(|n| n as f32)
                    .filter(|n| n.is_finite())
                    .ok_or_else(bad)?;
            }
            if components.iter().all(|n| *n == 0.0) {
                return Err(bad());
            }
            Some(Quat::from_array(components))
        }
    };
    let overhead = match params.get("overhead") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let required = |name| {
                num(value, name)?
                    .ok_or_else(|| RpcError::invalid_params(format!("`overhead.{name}` is required")))
            };
            let config = SpectatorOverheadConfig {
                height: required("height")?,
                min_height: required("min_height")?,
                max_height: required("max_height")?,
                pan_speed: required("pan_speed")?,
            };
            if config.min_height <= 0.0
                || config.max_height < config.min_height
                || config.height < config.min_height
                || config.height > config.max_height
                || config.pan_speed < 0.0
            {
                return Err(RpcError::invalid_params(
                    "`overhead` requires 0 < min_height <= height <= max_height and pan_speed >= 0",
                ));
            }
            Some(config)
        }
    };
    let mode = match params.get("mode") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().and_then(SpectatorMode::parse).ok_or_else(|| {
            RpcError::invalid_params("`mode` must be free_fly, follow, orbit, pov or overhead")
        })?),
    };
    let clear = crate::params::opt_bool(params, "clear_target")?.unwrap_or(false);
    let mut cam = camera_mut(world);
    if position.is_some() || yaw.is_some() || pitch.is_some() {
        let (old_position, old_yaw, old_pitch) = (cam.position(), cam.yaw(), cam.pitch());
        cam.set_pose(
            position.unwrap_or(old_position),
            yaw.unwrap_or(old_yaw),
            pitch.unwrap_or(old_pitch),
        );
    }
    if let Some(level) = level {
        cam.set_speed_level(level);
    }
    if clear {
        cam.clear_target();
    }
    if let Some(target) = target {
        cam.set_target(target);
    }
    if let Some(rotation) = target_rotation {
        let position = cam.target().unwrap_or(Vec3::ZERO);
        cam.set_target_pose(position, rotation);
    }
    if let Some(offset) = pov_offset {
        cam.set_pov_offset(offset);
    }
    if let Some(config) = overhead {
        cam.set_overhead_config(config);
    }
    if let Some(mode) = mode {
        cam.set_mode(mode);
    }
    if let Some(focus) = overhead_focus {
        cam.set_overhead_focus(focus);
    }
    Ok(camera_json(&cam))
}

/// `spectator.bind`: puts the spectator pose on a render camera (`camera`: an entity reference;
/// default the first entity with a `Camera`) and optionally follows an entity (`follow`;
/// `clear_follow` stops).
pub fn spectator_bind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad_entity = |what: &str, reason: String| {
        RpcError::new(
            protocol::NOT_FOUND,
            "spectator_bad_entity",
            format!("spectator.bind: {what}: {reason}"),
            "pass an entity id, {\"name\": \"...\"} or {\"component\": \"...\"} (scene.query_entities lists them)",
        )
    };
    let camera = match params.get("camera") {
        None | Some(Value::Null) => {
            let mut cameras: Vec<Entity> = world
                .query_filtered::<Entity, With<regia_render::Camera>>()
                .iter(world)
                .collect();
            cameras.sort_by_key(|entity| entity.to_bits());
            cameras
                .first()
                .copied()
                .ok_or_else(|| bad_entity("camera", "no entity has a Camera".to_string()))?
        }
        Some(reference) => {
            crate::scene::resolve_entity(world, reference).map_err(|e| bad_entity("camera", e))?
        }
    };
    let follow = match params.get("follow") {
        None | Some(Value::Null) => None,
        Some(reference) => {
            Some(crate::scene::resolve_entity(world, reference).map_err(|e| bad_entity("follow", e))?)
        }
    };
    let clear = crate::params::opt_bool(params, "clear_follow")?.unwrap_or(false);
    let driven = world.get_resource::<SpectatorDrive>().is_some();
    if !driven {
        world.init_resource::<SpectatorDrive>();
    }
    if !world.entity(camera).contains::<regia_scene::Transform>() {
        world.entity_mut(camera).insert(regia_scene::Transform::IDENTITY);
    }
    world.entity_mut(camera).insert(SpectatorView);
    let mut cam = camera_mut(world);
    if follow.is_some() && cam.mode() == SpectatorMode::FreeFly {
        cam.follow();
    }
    drop(cam);
    let mut drive = world.resource_mut::<SpectatorDrive>();
    if clear {
        drive.follow = None;
        drive.lost = None;
    }
    if follow.is_some() {
        drive.follow = follow;
        drive.lost = None;
    }
    let following = drive.follow.map(|e| e.to_bits());
    let mut out = camera_json(&camera_mut(world));
    out["camera"] = json!(camera.to_bits());
    out["follow"] = json!(following);
    out["plugin_installed"] = json!(driven);
    Ok(out)
}

/// `spectator.step`: advances the camera `frames` times by `dt` seconds under a constant input.
pub fn spectator_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let frames = match params.get("frames") {
        None | Some(Value::Null) => 1,
        Some(v) => v.as_u64().filter(|n| *n <= STEP_FRAMES_MAX).ok_or_else(|| {
            RpcError::invalid_params(format!("`frames` must be an integer 0..={STEP_FRAMES_MAX}"))
        })?,
    };
    let dt = num(params, "dt")?.unwrap_or(1.0 / 60.0);
    let movement = vec3(params, "movement")?.unwrap_or(Vec3::ZERO);
    let look = look_xy(params)?.unwrap_or(Vec3::ZERO);
    let input = SpectatorInput {
        movement,
        look: Vec2::new(look.x, look.y),
        speed_steps: int(params, "speed_steps")?.unwrap_or(0),
        zoom: num(params, "zoom")?.unwrap_or(0.0),
    };
    let mut cam = camera_mut(world);
    for index in 0..frames {
        // Speed steps and zoom are one-shot: only the first frame applies them.
        let frame = if index == 0 {
            input
        } else {
            SpectatorInput {
                speed_steps: 0,
                zoom: 0.0,
                ..input
            }
        };
        cam.update(&frame, dt);
    }
    let mut out = camera_json(&cam);
    out["frames"] = json!(frames);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use regia_framework::demo::{DemoHeader, DemoRecorder, DemoSim};

    use super::*;

    /// `docs/security/threat-model.md`: the file `demo.record_stop` writes keeps the project path rule,
    /// on every host, and the two extensions.
    #[test]
    fn a_demo_save_path_keeps_the_shared_rule() {
        assert_eq!(
            save_path("saved/demos/a.regiademo").expect("plain"),
            PathBuf::from("saved").join("demos").join("a.regiademo")
        );
        assert_eq!(
            save_path("saved\\a.frec").expect("either separator"),
            PathBuf::from("saved").join("a.frec")
        );
        for bad in [
            "",
            "a.txt",
            "/tmp/a.frec",
            "../a.frec",
            "a\\..\\..\\b.frec",
            "C:\\a.frec",
            "C:a.frec",
            "\\\\server\\share\\a.frec",
            "NUL.frec",
            "a.frec.",
            "a.frec:stream",
        ] {
            assert!(save_path(bad).is_err(), "{bad:?}");
        }
    }

    /// A counter that adds the first input of every step.
    #[derive(Default)]
    struct Counter(i64);

    impl DemoSim for Counter {
        fn step(&mut self, inputs: &[Vec3]) {
            self.0 += inputs.first().map_or(0, |v| v.x as i64);
        }
        fn state_hash(&self) -> u64 {
            self.0 as u64
        }
        fn snapshot(&self) -> Vec<u8> {
            self.0.to_le_bytes().to_vec()
        }
        fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
            let bytes: [u8; 8] = snapshot.try_into().map_err(|_| "bad length".to_string())?;
            self.0 = i64::from_le_bytes(bytes);
            Ok(())
        }
    }

    fn write_demo(dir: &std::path::Path, steps: u32) -> String {
        let mut sim = Counter::default();
        let header = DemoHeader::new("arena", 5, 60, 10).with_build("b1");
        let mut rec = DemoRecorder::begin(header, vec!["Move".to_string()], &sim);
        for _ in 0..steps {
            rec.record_step(&mut sim, &[Vec3::X]).expect("record");
        }
        let path = dir.join("t.regiademo");
        rec.finish(&sim).save(&path).expect("save");
        path.to_string_lossy().into_owned()
    }

    fn code_of(error: &RpcError) -> String {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn inspect_reports_header_and_keyframes() {
        let dir = regia_test_utils::unique_temp_dir("agent-demo-inspect");
        let path = write_demo(&dir, 25);
        let mut world = World::new();
        let out = demo_inspect(&mut world, &json!({ "path": path })).expect("inspect");
        assert_eq!(out["steps"], 25);
        assert_eq!(out["header"]["level"], "arena");
        assert_eq!(out["header"]["build_hash"], "b1");
        assert_eq!(out["header"]["seed"], "5");
        assert_eq!(out["keyframe_count"], 4);
        assert_eq!(out["keyframes"][1]["tick"], 10);
        assert_eq!(out["keyframes"][1]["snapshot_bytes"], 8);
        assert_eq!(out["keyframes_truncated"], false);
        assert_eq!(out["engine_matches"], true);
        assert!((out["duration_secs"].as_f64().unwrap_or(0.0) - 25.0 / 60.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inspect_error_classes() {
        let dir = regia_test_utils::unique_temp_dir("agent-demo-errors");
        let path = write_demo(&dir, 12);
        let mut world = World::new();
        let missing = demo_inspect(&mut world, &json!({})).expect_err("path");
        assert_eq!(code_of(&missing), "F0002");
        let io = demo_inspect(
            &mut world,
            &json!({ "path": dir.join("none.regiademo").to_string_lossy() }),
        )
        .expect_err("io");
        assert_eq!(code_of(&io), "demo_io");
        let text = std::fs::read_to_string(&path).expect("read");
        let cut = dir.join("cut.regiademo");
        std::fs::write(&cut, &text[..text.len() / 2]).expect("write");
        let truncated =
            demo_inspect(&mut world, &json!({ "path": cut.to_string_lossy() })).expect_err("truncated");
        assert_eq!(code_of(&truncated), "demo_truncated");
        let junk = dir.join("junk.regiademo");
        std::fs::write(&junk, "nope\n// regia-demo-end\n").expect("write");
        let parse = demo_inspect(&mut world, &json!({ "path": junk.to_string_lossy() })).expect_err("parse");
        assert_eq!(code_of(&parse), "demo_parse");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// audit-agent-sec (High): `path` was read unconfined (`/dev/zero` grew until the world thread
    /// was killed; any host file's length came back in the truncation error).
    #[test]
    fn demo_paths_are_confined_and_only_regular_files_are_read() {
        let dir = regia_test_utils::unique_temp_dir("agent-demo-confine");
        let path = write_demo(&dir, 12);
        let mut world = World::new();
        for outside in [
            "/etc/passwd",
            "/dev/zero",
            "../t.regiademo",
            "a/../../t.regiademo",
        ] {
            for method in [demo_inspect, demo_seek_plan] {
                let error = method(&mut world, &json!({ "path": outside, "tick": 0 })).expect_err(outside);
                assert_eq!(code_of(&error), "F0002", "{outside}");
                assert!(!error.message.contains("bytes"), "{}", error.message);
            }
        }
        // A directory under an allowed root is an io error, not a read.
        let not_file = demo_inspect(&mut world, &json!({ "path": dir.to_string_lossy() })).expect_err("dir");
        assert_eq!(code_of(&not_file), "demo_io");
        #[cfg(unix)]
        {
            // A link under the temp dir that leads out of every root is refused before the read.
            std::os::unix::fs::symlink("/etc", dir.join("etc")).expect("symlink");
            let linked = dir.join("etc").join("passwd");
            let error = demo_inspect(&mut world, &json!({ "path": linked.to_string_lossy() }))
                .expect_err("symlink out");
            assert_eq!(code_of(&error), "F0002");
        }
        assert!(demo_inspect(&mut world, &json!({ "path": path })).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_plan_names_the_keyframe_and_the_replay_length() {
        let dir = regia_test_utils::unique_temp_dir("agent-demo-plan");
        let path = write_demo(&dir, 25);
        let mut world = World::new();
        let plan = demo_seek_plan(&mut world, &json!({ "path": path, "tick": 17 })).expect("plan");
        assert_eq!(plan["keyframe_tick"], 10);
        assert_eq!(plan["steps_to_replay"], 7);
        assert_eq!(plan["exact_keyframe"], false);
        let exact = demo_seek_plan(&mut world, &json!({ "path": path, "tick": 20 })).expect("plan");
        assert_eq!(
            (exact["keyframe_tick"].clone(), exact["exact_keyframe"].clone()),
            (json!(20), json!(true))
        );
        let late = demo_seek_plan(&mut world, &json!({ "path": path, "tick": 26 })).expect_err("range");
        assert_eq!(code_of(&late), "demo_tick_out_of_range");
        let bad = demo_seek_plan(&mut world, &json!({ "path": path })).expect_err("tick");
        assert_eq!(code_of(&bad), "F0002");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spectator_set_step_get_round_trip() {
        let mut world = World::new();
        let fresh = spectator_get(&mut world, &json!({})).expect("get");
        assert_eq!(fresh["mode"], "free_fly");
        assert_eq!(fresh["speed_level"], 0);
        let set = spectator_set(
            &mut world,
            &json!({ "position": [1.0, 2.0, 3.0], "speed_level": 2, "yaw": 0.0 }),
        )
        .expect("set");
        assert_eq!(set["position"], json!([1.0, 2.0, 3.0]));
        assert_eq!(set["speed_level"], 2);
        // 1 s forward at 8 * 1.25^2 = 12.5 m/s, in 4 frames of 0.25 s.
        let step = spectator_step(
            &mut world,
            &json!({ "frames": 4, "dt": 0.25, "movement": [0.0, 0.0, 1.0] }),
        )
        .expect("step");
        assert_eq!(step["frames"], 4);
        let z = step["position"][2].as_f64().unwrap_or(0.0);
        assert!((z - (3.0 - 12.5)).abs() < 1e-4, "{z}");
        let got = spectator_get(&mut world, &json!({})).expect("get");
        assert_eq!(got["position"], step["position"]);
    }

    #[test]
    fn spectator_follow_converges_through_the_protocol() {
        let mut world = World::new();
        let set = spectator_set(
            &mut world,
            &json!({ "mode": "follow", "target": [10.0, 0.0, 0.0] }),
        )
        .expect("set");
        assert_eq!(set["mode"], "follow");
        assert_eq!(set["target"], json!([10.0, 0.0, 0.0]));
        let step = spectator_step(&mut world, &json!({ "frames": 600, "dt": 0.016 })).expect("step");
        let anchor_x = step["anchor"][0].as_f64().unwrap_or(0.0);
        assert!((anchor_x - 10.0).abs() < 1e-2, "{anchor_x}");
        let cleared =
            spectator_set(&mut world, &json!({ "clear_target": true, "mode": "free_fly" })).expect("set");
        assert_eq!(cleared["target"], Value::Null);
        assert_eq!(cleared["mode"], "free_fly");
    }

    #[test]
    fn spectator_rejects_bad_parameters() {
        let mut world = World::new();
        for params in [
            json!({ "mode": "unknown_mode" }),
            json!({ "position": [1.0, 2.0] }),
            json!({ "position": [1.0, "x", 3.0] }),
            json!({ "yaw": "left" }),
            json!({ "speed_level": 1.5 }),
            json!({ "target": 4 }),
            json!({ "yaw": 1e300 }),
            json!({ "pitch": -1e39 }),
            json!({ "position": [0.0, 1e39, 0.0] }),
            json!({ "target_rotation": [0, 0, 0, 0] }),
            json!({ "target_rotation": [0, 0, 0] }),
            json!({ "target_rotation": [0, 0, 0, 1e39] }),
            json!({ "pov_offset": [0, "up", 0] }),
            json!({ "overhead_focus": false }),
            json!({ "overhead": { "height": 20, "min_height": 0, "max_height": 50, "pan_speed": 1 } }),
            json!({ "overhead": { "height": 20, "min_height": 2, "max_height": 10, "pan_speed": 1 } }),
            json!({ "overhead": { "height": 20, "min_height": 2, "max_height": 50, "pan_speed": -1 } }),
            json!({ "overhead": { "height": 20 } }),
            json!({ "overhead": [] }),
        ] {
            let error = spectator_set(&mut world, &params).expect_err("bad");
            assert_eq!(code_of(&error), "F0002", "{params}");
        }
        for params in [
            json!({ "frames": 100_001 }),
            json!({ "frames": -1 }),
            json!({ "dt": "x" }),
            json!({ "movement": [1, 2, 3, 4] }),
            json!({ "look": [0.1] }),
            json!({ "look": [0.1, 0.2, 0.3, 0.4] }),
            // Finite in f64 but infinite in f32 (ADR-1660).
            json!({ "dt": 1e300 }),
            json!({ "movement": [1e39, 0.0, 0.0] }),
            json!({ "look": [1e300, 0.0] }),
        ] {
            let error = spectator_step(&mut world, &params).expect_err("bad");
            assert_eq!(code_of(&error), "F0002", "{params}");
        }
    }

    #[test]
    fn spectator_new_settings_round_trip_and_reject_atomically() {
        let mut world = World::new();
        let out = spectator_set(
            &mut world,
            &json!({
                "mode": "overhead", "target": [3, 4, 5],
                "target_rotation": [0, 0, 0, 2], "pov_offset": [0, 1.5, 0],
                "overhead": { "height": 20, "min_height": 2, "max_height": 80, "pan_speed": 0.5 }
            }),
        )
        .expect("set");
        assert_eq!(out["overhead_focus"], json!([3.0, 4.0, 5.0]));
        assert_eq!(out["target_rotation"], json!([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(out["pov_offset"], json!([0.0, 1.5, 0.0]));
        assert_eq!(out["overhead"]["max_height"], 80.0);
        assert_eq!(out["overhead_height"], 20.0);
        let before = world.resource::<SpectatorCamera>().clone();
        let error = spectator_set(
            &mut world,
            &json!({
                "position": [99, 99, 99], "mode": "pov", "target_rotation": [0, 0, 0, 0]
            }),
        )
        .expect_err("invalid quaternion");
        assert_eq!(code_of(&error), "F0002");
        assert_eq!(world.resource::<SpectatorCamera>(), &before);
        let out = spectator_step(&mut world, &json!({ "frames": 3, "zoom": 5.0 })).expect("step");
        assert_eq!(out["overhead_height"], 25.0, "zoom applies only on first frame");
        assert_eq!(
            spectator_get(&mut world, &json!({})).expect("get")["mode"],
            "overhead"
        );
    }

    /// `look` is yaw and pitch. A third number is ignored and does not move the camera.
    #[test]
    fn spectator_step_look_accepts_a_yaw_pitch_pair() {
        let mut pair = World::new();
        let mut triple = World::new();
        let a = spectator_step(&mut pair, &json!({ "look": [0.25, -0.1], "dt": 1.0 })).expect("pair");
        let b = spectator_step(&mut triple, &json!({ "look": [0.25, -0.1, 50.0], "dt": 1.0 }))
            .expect("third ignored");
        assert_eq!(a["yaw"], b["yaw"]);
        assert_eq!(a["pitch"], b["pitch"]);
    }

    // -- The live driver (ADR-1878) ------------------------------------------------------------

    use regia_framework::demo_world::{DemoTracked, DemoWorldPlugin};

    #[derive(Component, Reflect, Default, Clone)]
    #[reflect(Component, Default)]
    struct Ticks(i64);

    fn tick_counter(actions: Res<regia_input::Actions>, mut ticks: Query<&mut Ticks>) {
        let step = 1 + (actions.value("Move").x * 4.0) as i64;
        for mut t in &mut ticks {
            t.0 += step;
        }
    }

    fn live_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins((
            regia_input::InputPlugin::new(
                regia_input::InputConfig::new().action("Move", regia_input::ActionKind::Axis1D),
            ),
            DemoWorldPlugin,
        ));
        app.register_type::<Ticks>();
        app.add_systems(FixedUpdate, tick_counter.after(regia_input::InputSet::Evaluation));
        enable_fixed_step(&mut app, 60.0);
        app.world_mut().spawn((DemoTracked, Ticks(0)));
        app.update();
        app
    }

    fn ticks(app: &mut App) -> i64 {
        let world = app.world_mut();
        world.query::<&Ticks>().iter(world).map(|t| t.0).sum()
    }

    fn status(app: &mut App) -> Value {
        demo_status_method(app.world_mut(), &json!({})).expect("status")
    }

    #[test]
    fn live_methods_record_play_seek_and_pause() {
        let mut app = live_app();
        let out = demo_record_start(
            app.world_mut(),
            &json!({ "level": "yard", "seed": 3, "keyframe_interval": 5 }),
        )
        .expect("start");
        assert_eq!(out["queued"], true);
        app.update();
        assert_eq!(status(&mut app)["mode"], "recording");
        let busy = demo_record_start(app.world_mut(), &json!({})).expect_err("busy");
        assert_eq!(code_of(&busy), "demo_busy");
        assert_eq!(
            code_of(&demo_seek(app.world_mut(), &json!({ "tick": 1 })).expect_err("busy")),
            "demo_busy"
        );
        let start = ticks(&mut app);
        let mut after = vec![start];
        for frame in 0..12 {
            let next = app.world().resource::<Time>().frame_count() + 1;
            let value = Vec3::new(if frame % 3 == 0 { 1.0 } else { 0.0 }, 0.0, 0.0);
            app.world_mut()
                .resource_mut::<regia_input::InputInjection>()
                .schedule("Move".to_string(), value, next, 1);
            if frame == 11 {
                demo_record_stop(app.world_mut(), &json!({})).expect("stop");
            }
            app.update();
            after.push(ticks(&mut app));
        }
        let st = status(&mut app);
        assert_eq!(st["mode"], "idle");
        assert!(st["last_error"].is_null(), "{st}");

        // Replay the recording (no path) and seek around in it.
        let play = demo_play(app.world_mut(), &json!({})).expect("play");
        assert_eq!(play["steps"], 12);
        app.update();
        let st = status(&mut app);
        assert_eq!(
            (st["mode"].as_str(), st["tick"].as_u64()),
            (Some("playing"), Some(0))
        );
        assert_eq!(ticks(&mut app), start, "play restores the first keyframe");
        assert_eq!(st["keyframes"], json!([0, 5, 10, 12]));

        demo_pause(app.world_mut(), &json!({ "paused": true })).expect("pause");
        let seek = demo_seek(app.world_mut(), &json!({ "tick": 7 })).expect("seek");
        assert_eq!(
            (seek["keyframe_tick"].as_u64(), seek["steps_to_replay"].as_u64()),
            (Some(5), Some(2))
        );
        app.update();
        let st = status(&mut app);
        assert_eq!(
            (st["mode"].as_str(), st["tick"].as_u64()),
            (Some("paused"), Some(7))
        );
        assert_eq!(ticks(&mut app), after[7], "the seek lands on the recorded state");
        assert_eq!(st["last_seek"]["keyframe_tick"], 5);
        let late = demo_seek(app.world_mut(), &json!({ "tick": 13 })).expect_err("range");
        assert_eq!(code_of(&late), "demo_tick_out_of_range");
        demo_pause(app.world_mut(), &json!({ "paused": false })).expect("resume");
        for _ in 0..8 {
            app.update();
        }
        let st = status(&mut app);
        assert_eq!(
            (st["mode"].as_str(), st["tick"].as_u64()),
            (Some("finished"), Some(12))
        );
        assert_eq!(ticks(&mut app), after[12]);
        assert!(st["diverged_at"].is_null());
    }

    /// sec-review-assets-agent (ADR-5869): `demo.record_stop` passed its checked relative path to
    /// the framework, which wrote it against the working directory (the user's home when a desktop
    /// launcher starts the game), not the project, and through any symlinked folder there. It is
    /// resolved under the project root now, and `saved_to` still answers it project-relative.
    #[test]
    fn a_saved_demo_lands_under_the_project_root() {
        let project = regia_test_utils::unique_temp_dir("demo-save-root");
        let mut app = live_app();
        app.insert_resource(crate::AgentSettings {
            project_dir: Some(project.clone()),
        });
        demo_record_start(app.world_mut(), &json!({ "level": "yard" })).expect("start");
        app.update();
        app.update();
        let rel = "saved/demos/sec-review-run.regiademo";
        std::fs::create_dir_all(project.join("saved/demos")).expect("dirs");
        demo_record_stop(app.world_mut(), &json!({ "path": rel })).expect("stop");
        app.update();
        assert!(
            project.join(rel).is_file(),
            "the demo is written under the project root"
        );
        assert!(
            !std::env::current_dir().expect("cwd").join(rel).exists(),
            "nothing is written under the working directory"
        );
        let saved = status(&mut app)["saved_to"].as_str().map(str::to_owned);
        assert_eq!(
            saved.map(|s| s.replace('\\', "/")).as_deref(),
            Some(rel),
            "saved_to stays project-relative"
        );
        #[cfg(unix)]
        {
            let outside = regia_test_utils::unique_temp_dir("demo-save-outside");
            std::os::unix::fs::symlink(&outside, project.join("out")).expect("link");
            demo_record_start(app.world_mut(), &json!({ "level": "yard" })).expect("start again");
            app.update();
            let err = demo_record_stop(app.world_mut(), &json!({ "path": "out/x.regiademo" }))
                .expect_err("a symlinked folder leading out is refused");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
            let _ = std::fs::remove_dir_all(&outside);
        }
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn live_methods_play_a_file_and_report_their_errors() {
        let mut bare = World::new();
        for (name, result) in [
            ("demo.status", demo_status_method(&mut bare, &json!({}))),
            ("demo.record_start", demo_record_start(&mut bare, &json!({}))),
            ("demo.record_stop", demo_record_stop(&mut bare, &json!({}))),
            ("demo.play", demo_play(&mut bare, &json!({}))),
            ("demo.pause", demo_pause(&mut bare, &json!({}))),
            ("demo.seek", demo_seek(&mut bare, &json!({ "tick": 0 }))),
            ("demo.scrub_bar", demo_scrub_bar(&mut bare, &json!({}))),
        ] {
            assert_eq!(code_of(&result.expect_err(name)), "demo_not_installed", "{name}");
        }
        let mut app = live_app();
        let world = app.world_mut();
        assert_eq!(
            code_of(&demo_record_stop(world, &json!({})).expect_err("idle")),
            "demo_no_demo"
        );
        assert_eq!(
            code_of(&demo_pause(world, &json!({})).expect_err("none")),
            "demo_no_demo"
        );
        assert_eq!(
            code_of(&demo_play(world, &json!({})).expect_err("none")),
            "demo_no_demo"
        );
        assert_eq!(code_of(&demo_seek(world, &json!({})).expect_err("tick")), "F0002");
        for bad in [
            "/tmp/x.frec",
            "../x.frec",
            "saved/../../x.regiademo",
            "saved/x.txt",
            "",
        ] {
            assert_eq!(code_of(&save_path(bad).expect_err(bad)), "demo_bad_path", "{bad}");
        }
        assert!(save_path("saved/demos/run-1.frec").is_ok());
        assert!(save_path("run.regiademo").is_ok());

        // A .frec on disk plays; a cut one is refused with the demo error code.
        let dir = regia_test_utils::unique_temp_dir("agent-demo-play");
        let mut sim = Counter::default();
        let mut rec = DemoRecorder::begin(DemoHeader::new("l", 0, 60, 2), vec!["Move".to_string()], &sim);
        for _ in 0..4 {
            rec.record_step(&mut sim, &[Vec3::X]).expect("record");
        }
        let demo = rec.finish(&sim);
        let frec = dir.join("m.frec");
        regia_framework::input_recording::save_frec(&demo, &frec).expect("save");
        let inspected =
            demo_inspect(world, &json!({ "path": frec.to_string_lossy() })).expect("inspect frec");
        assert_eq!(inspected["steps"], 4);
        assert_eq!(inspected["has_timestamps"], false);
        let bytes = std::fs::read(&frec).expect("read");
        let cut = dir.join("cut.frec");
        std::fs::write(&cut, &bytes[..bytes.len() - 3]).expect("write");
        let err = demo_play(world, &json!({ "path": cut.to_string_lossy() })).expect_err("cut");
        assert_eq!(code_of(&err), "demo_truncated");
        let mut flipped = bytes.clone();
        flipped[20] ^= 1;
        let bad = dir.join("bad.frec");
        std::fs::write(&bad, &flipped).expect("write");
        let err = demo_inspect(world, &json!({ "path": bad.to_string_lossy() })).expect_err("crc");
        assert_eq!(code_of(&err), "demo_corrupt");
        let out = demo_play(world, &json!({ "path": frec.to_string_lossy() })).expect("play");
        assert_eq!(out["steps"], 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scrub_bar_opens_and_closes() {
        let mut world = World::new();
        world.init_resource::<DemoScrubBar>();
        assert_eq!(
            demo_scrub_bar(&mut world, &json!({})).expect("open")["open"],
            true
        );
        assert!(world.resource::<DemoScrubBar>().open);
        demo_scrub_bar(&mut world, &json!({ "open": false })).expect("close");
        assert!(!world.resource::<DemoScrubBar>().open);
        assert_eq!(
            code_of(&demo_scrub_bar(&mut world, &json!({ "open": 1 })).expect_err("bad")),
            "F0002"
        );
    }

    #[test]
    fn spectator_bind_marks_the_camera_and_follows() {
        let mut world = World::new();
        world.init_resource::<regia_ecs::reflect::AppTypeRegistry>();
        let camera = world.spawn(regia_render::Camera::default()).id();
        let hero = world
            .spawn((
                Name::new("Hero"),
                regia_scene::Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
            ))
            .id();
        let out = spectator_bind(&mut world, &json!({ "follow": { "name": "Hero" } })).expect("bind");
        assert_eq!(out["camera"], camera.to_bits());
        assert_eq!(out["follow"], hero.to_bits());
        assert_eq!(out["mode"], "follow");
        assert_eq!(out["plugin_installed"], false);
        assert!(world.entity(camera).contains::<SpectatorView>());
        assert!(world.entity(camera).contains::<regia_scene::Transform>());
        let out = spectator_bind(
            &mut world,
            &json!({ "camera": camera.to_bits(), "clear_follow": true }),
        )
        .expect("clear");
        assert!(out["follow"].is_null());
        assert_eq!(out["plugin_installed"], true, "the drive resource exists now");
        let err = spectator_bind(&mut world, &json!({ "follow": { "name": "Nobody" } })).expect_err("bad");
        assert_eq!(code_of(&err), "spectator_bad_entity");
        let mut empty = World::new();
        let err = spectator_bind(&mut empty, &json!({})).expect_err("no camera");
        assert_eq!(code_of(&err), "spectator_bad_entity");
    }
}
