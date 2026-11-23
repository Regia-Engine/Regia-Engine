// SPDX-License-Identifier: MPL-2.0
//! Agent methods for per-camera shake sensors (ADR-1619, book/agents/camera-shake.md):
//! `camera.shake` sends a plain shake, `camera.shake_sensor` reads the cameras' sensors and
//! running shakes, `camera.set_shake_sensor` changes or removes a camera's sensor.
//!
//! Handlers are plain functions over `&mut World`. A world without `CameraPlugin` answers
//! `UNSUPPORTED`. Categorical mistakes carry `data.reason` (lowercase codes listed on the book
//! page).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::sensor::{
    NAMED_SHAKES, ShakeAxes, ShakeClock, ShakeCombine, ShakeEmit, ShakeField, ShakeKind, ShakePreset,
    ShakeReceiver, ShakeSensor, effective_sensor, named_shake,
};
use regia_framework::camera::{CameraRig, CinematicCamera, Shake};
use regia_framework::ground_shock::GroundShock;
use regia_framework::shock_shake::ShockShake;
use regia_framework::shots::ShotPlayer;
use regia_scene::Transform;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most cameras `camera.shake_sensor` and `camera.shake` list.
const MAX_LISTED: usize = 256;

fn unsupported() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0016",
        "no camera shake sensors in this app",
        "add regia_framework::camera::CameraPlugin",
    )
}

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn number(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?
                as f32;
            if !n.is_finite() {
                return Err(RpcError::invalid_params(format!("`{field}` must be finite")));
            }
            Ok(Some(n))
        }
    }
}

/// A number that must be above zero (or at least zero with `allow_zero`).
fn positive(params: &Value, field: &str, allow_zero: bool) -> Result<Option<f32>, RpcError> {
    match number(params, field)? {
        Some(n) if n > 0.0 || (allow_zero && n == 0.0) => Ok(Some(n)),
        Some(_) if allow_zero => Err(RpcError::invalid_params(format!(
            "`{field}` must not be negative"
        ))),
        Some(_) => Err(RpcError::invalid_params(format!("`{field}` must be positive"))),
        None => Ok(None),
    }
}

fn text<'a>(params: &'a Value, field: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a string"))),
    }
}

fn channels(params: &Value) -> Result<Option<u32>, RpcError> {
    match params.get("channels") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params("`channels` must be an integer bitmask in 0..=4294967295")
            }),
    }
}

fn origin(params: &Value) -> Result<Option<Vec3>, RpcError> {
    let invalid = || RpcError::invalid_params("`origin` must be [x, y, z], three finite numbers");
    match params.get("origin") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let items = value.as_array().filter(|a| a.len() == 3).ok_or_else(invalid)?;
            let mut out = [0.0f32; 3];
            for (slot, item) in out.iter_mut().zip(items) {
                let n = item.as_f64().ok_or_else(invalid)? as f32;
                if !n.is_finite() {
                    return Err(invalid());
                }
                *slot = n;
            }
            Ok(Some(Vec3::from_array(out)))
        }
    }
}

fn entity_param(params: &Value, required: bool) -> Result<Option<Entity>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) if required => Err(RpcError::invalid_params(
            "`entity` (the camera's id from scene.query_entities) is required",
        )),
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        }
    }
}

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn sensor_json(sensor: &ShakeSensor) -> Value {
    json!({
        "channels": sensor.channels,
        "gain": sensor.gain,
        "effective_gain": sensor.effective_gain(),
        "combine": sensor.combine.name(),
        "axes": sensor.axes.name(),
        "clock": sensor.clock.name(),
    })
}

fn kind_json(kind: &ShakeKind) -> Value {
    json!({
        "kind": kind.name(),
        "peak": kind.peak(),
        "duration_s": kind.duration_s(),
    })
}

fn receiver_json(receiver: &ShakeReceiver, frame: u64) -> Value {
    let pose = receiver.pose();
    json!({
        "pending": receiver.pending().iter().map(|p| {
            let mut entry = kind_json(&p.kind);
            if let Value::Object(map) = &mut entry {
                map.insert("emitted_frame".into(), json!(p.emitted_frame));
                map.insert("arrival_frame".into(), json!(p.arrival_frame));
                map.insert("frames_left".into(), json!(p.arrival_frame.saturating_sub(frame)));
                map.insert("distance_m".into(), json!(p.distance_m));
            }
            entry
        }).collect::<Vec<_>>(),
        "active": receiver.active().iter().map(|a| {
            let mut entry = kind_json(&a.kind);
            if let Value::Object(map) = &mut entry {
                map.insert("elapsed_s".into(), json!(a.elapsed_s));
            }
            entry
        }).collect::<Vec<_>>(),
        "received": receiver.received(),
        "fired": receiver.fired(),
        "pose": {
            "offset": vec3_json(pose.offset),
            "pitch": pose.pitch,
            "yaw": pose.yaw,
            "roll": pose.roll,
        },
    })
}

fn field_json(field: &ShakeField) -> Value {
    json!({
        "frame": field.frame(),
        "fixed_hz": field.fixed_hz(),
        "emitted": field.emitted(),
        "delivered": field.delivered(),
        "contact_emitted": field.contact_emitted(),
    })
}

/// The camera the shot player drives, if any.
fn driven_camera(world: &World) -> Option<Entity> {
    world
        .get_resource::<ShotPlayer>()
        .and_then(ShotPlayer::driven_camera)
}

/// Whether a cinematic player drives `entity`: it is the shot player's camera (`driven`) or carries
/// a `CinematicCamera` marker (a sequence takeover's camera, ADR-1614). Such a camera defaults to a
/// sensor, as the framework's shake systems count it.
fn is_shot_driven(world: &World, entity: Entity, driven: Option<Entity>) -> bool {
    driven == Some(entity) || world.get::<CinematicCamera>(entity).is_some()
}

/// One camera's sensor and shakes.
fn camera_json(world: &World, entity: Entity, frame: u64, driven: Option<Entity>) -> Value {
    let explicit = world.get::<ShakeSensor>(entity);
    let rig = world.get::<CameraRig>(entity).is_some();
    let shot_driven = is_shot_driven(world, entity, driven);
    let sensor = effective_sensor(explicit, rig, shot_driven);
    json!({
        "entity": entity.to_bits(),
        "rig": rig,
        "render_camera": world.get::<regia_render::Camera>(entity).is_some(),
        "shot_driven": shot_driven,
        "explicit": explicit.is_some(),
        "sensor": sensor.as_ref().map(sensor_json),
        "receiver": world.get::<ShakeReceiver>(entity).map(|r| receiver_json(r, frame)),
    })
}

/// Every entity that is a camera for shake purposes: a rig, a render camera, or an entity with a
/// sensor or receiver; sorted by id.
fn cameras(world: &mut World) -> Vec<Entity> {
    let all: Vec<Entity> = world.query::<Entity>().iter(world).collect();
    let mut out: Vec<Entity> = all
        .into_iter()
        .filter(|&entity| {
            world.get::<CameraRig>(entity).is_some()
                || world.get::<regia_render::Camera>(entity).is_some()
                || world.get::<ShakeSensor>(entity).is_some()
                || world.get::<ShakeReceiver>(entity).is_some()
        })
        .collect();
    out.sort_by_key(|e| e.to_bits());
    out
}

/// `camera.shake_sensor { entity? }`: one camera's sensor and shakes, or every camera's.
pub fn camera_shake_sensor(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(field) = world.get_resource::<ShakeField>().cloned() else {
        return Err(unsupported());
    };
    let driven = driven_camera(world);
    if let Some(entity) = entity_param(params, false)? {
        if world.get_entity(entity).is_err() {
            return Err(RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "call camera.shake_sensor without `entity` to list the cameras",
            ));
        }
        return Ok(json!({
            "field": field_json(&field),
            "camera": camera_json(world, entity, field.frame(), driven),
        }));
    }
    let all = cameras(world);
    let listed: Vec<Value> = all
        .iter()
        .take(MAX_LISTED)
        .map(|&entity| camera_json(world, entity, field.frame(), driven))
        .collect();
    Ok(json!({
        "field": field_json(&field),
        "count": all.len(),
        "truncated": all.len() > listed.len(),
        "cameras": listed,
    }))
}

/// `camera.set_shake_sensor { entity, channels?, gain?, combine?, axes?, clock?, remove? }`:
/// changes a camera's sensor (starting from the one it feels with now), or removes it.
pub fn camera_set_shake_sensor(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<ShakeField>().is_none() {
        return Err(unsupported());
    }
    let Some(entity) = entity_param(params, true)? else {
        return Err(RpcError::invalid_params("`entity` is required"));
    };
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "call camera.shake_sensor to list the cameras",
        ));
    }
    if world.get::<Transform>(entity).is_none() {
        return Err(refused(
            "shake_not_a_camera",
            format!(
                "entity {} has no Transform, so nothing can shake",
                entity.to_bits()
            ),
            "pick a camera entity from camera.shake_sensor",
        ));
    }
    let remove = match params.get("remove") {
        None | Some(Value::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`remove` must be a boolean"))?,
    };
    let channels = channels(params)?;
    let gain = number(params, "gain")?;
    if gain.is_some_and(|g| !(0.0..=ShakeSensor::MAX_GAIN).contains(&g)) {
        return Err(RpcError::invalid_params("`gain` must be in 0..=100"));
    }
    let combine = text(params, "combine")?
        .map(|name| {
            ShakeCombine::from_name(name).ok_or_else(|| {
                refused(
                    "shake_unknown_combine",
                    format!("unknown combine {name:?}"),
                    "use add or max",
                )
            })
        })
        .transpose()?;
    let axes = text(params, "axes")?
        .map(|name| {
            ShakeAxes::from_name(name).ok_or_else(|| {
                refused(
                    "shake_unknown_axes",
                    format!("unknown axes {name:?}"),
                    "use world or camera",
                )
            })
        })
        .transpose()?;
    let clock = text(params, "clock")?
        .map(|name| {
            ShakeClock::from_name(name).ok_or_else(|| {
                refused(
                    "shake_unknown_clock",
                    format!("unknown clock {name:?}"),
                    "use gameplay or unscaled",
                )
            })
        })
        .transpose()?;
    let changes =
        channels.is_some() || gain.is_some() || combine.is_some() || axes.is_some() || clock.is_some();
    if remove && changes {
        return Err(RpcError::invalid_params(
            "`remove` cannot be combined with sensor fields",
        ));
    }

    let driven = driven_camera(world);
    let rig = world.get::<CameraRig>(entity).is_some();
    let shot_driven = is_shot_driven(world, entity, driven);
    let before = effective_sensor(world.get::<ShakeSensor>(entity), rig, shot_driven);
    let mut entity_mut = world.entity_mut(entity);
    if remove {
        entity_mut.remove::<ShakeSensor>();
    } else {
        let mut sensor = before.unwrap_or_default();
        if let Some(channels) = channels {
            sensor.channels = channels;
        }
        if let Some(gain) = gain {
            sensor.gain = gain;
        }
        if let Some(combine) = combine {
            sensor.combine = combine;
        }
        if let Some(axes) = axes {
            sensor.axes = axes;
        }
        if let Some(clock) = clock {
            sensor.clock = clock;
        }
        entity_mut.insert(sensor);
    }
    let explicit = world.get::<ShakeSensor>(entity).copied();
    let after = effective_sensor(explicit.as_ref(), rig, shot_driven);
    Ok(json!({
        "entity": entity.to_bits(),
        "removed": remove,
        "explicit": explicit.is_some(),
        "before": before.as_ref().map(sensor_json),
        "after": after.as_ref().map(sensor_json),
    }))
}

/// The preset `camera.shake` plays, with the noise overrides applied.
fn preset_from(params: &Value) -> Result<(String, ShakePreset), RpcError> {
    let name = text(params, "preset")?.unwrap_or("explosion");
    let overrides = ["amplitude", "frequency_hz", "duration_s", "falloff"];
    let overridden = overrides
        .iter()
        .any(|field| params.get(*field).is_some_and(|v| !v.is_null()));
    let preset = match name {
        "blast" | "ground" => {
            if overridden {
                return Err(refused(
                    "shake_override_not_noise",
                    format!(
                        "amplitude, frequency_hz, duration_s and falloff apply to noise presets, not {name}"
                    ),
                    "drop the overrides, or pick bump, impact, rumble or explosion",
                ));
            }
            if name == "blast" {
                ShakePreset::Blast(ShockShake::showcase())
            } else {
                let mut ground = GroundShock::default();
                if let Some(yield_kt) = positive(params, "yield_kt", false)? {
                    ground.yield_kt = yield_kt;
                }
                ShakePreset::Ground(ground)
            }
        }
        _ => {
            let Some(mut shake) = named_shake(name) else {
                return Err(refused(
                    "shake_unknown_preset",
                    format!("unknown preset {name:?}"),
                    "use bump, impact, rumble, explosion, blast or ground",
                ));
            };
            if let Some(amplitude) = positive(params, "amplitude", true)? {
                shake.amplitude = amplitude;
            }
            if let Some(frequency) = positive(params, "frequency_hz", false)? {
                shake.frequency_hz = frequency;
            }
            if let Some(duration) = positive(params, "duration_s", false)? {
                shake.duration_secs = duration;
            }
            if let Some(falloff) = positive(params, "falloff", true)? {
                shake.falloff = falloff;
            }
            ShakePreset::Noise(shake)
        }
    };
    Ok((name.to_string(), preset))
}

/// `camera.shake { ..., player }`: a noise shake for local player `index`'s camera director only
/// (spec 14 § B5, ADR-7263): the cameras marked `PlayerCamera(index)` feel it, no other camera.
fn shake_player(world: &mut World, index: u64, name: String, preset: ShakePreset) -> Result<Value, RpcError> {
    let ShakePreset::Noise(shake) = preset else {
        return Err(refused(
            "shake_player_noise_only",
            format!("preset {name} cannot be routed to a player: a player's director plays noise shakes"),
            "use bump, impact, rumble or explosion with player, or drop player to send blast or ground",
        ));
    };
    let not_local = || {
        refused(
            "player_not_local",
            format!("player {index} is not a local player"),
            "camera.get lists the local players under players",
        )
    };
    let index = u8::try_from(index).map_err(|_| not_local())?;
    let mut manager = world
        .get_resource_mut::<regia_framework::player::PlayerManager>()
        .ok_or_else(not_local)?;
    let director = manager.camera_director_mut(index).ok_or_else(not_local)?;
    director.shake(shake);
    let shakes = director.shake_count();
    Ok(json!({
        "emitted": true,
        "preset": name,
        "kind": "noise",
        "shake": {
            "amplitude": shake.amplitude,
            "frequency_hz": shake.frequency_hz,
            "duration_s": shake.duration_secs,
            "falloff": shake.falloff,
        },
        "player": index,
        "shakes": shakes,
    }))
}

/// `camera.shake { preset?, amplitude?, frequency_hz?, duration_s?, falloff?, origin?, channels?,
/// falloff_radius_m?, travel_speed_m_s?, yield_kt?, player? }`: sends a shake to the listening
/// cameras (it is read next frame) and reports who will feel what; with `player`, to that local
/// player's director instead.
pub fn camera_shake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(field) = world.get_resource::<ShakeField>().cloned() else {
        return Err(unsupported());
    };
    let (name, preset) = preset_from(params)?;
    if let Some(index) = crate::params::opt_u64(params, "player")? {
        return shake_player(world, index, name, preset);
    }
    let channels = channels(params)?.unwrap_or(ShakeSensor::DEFAULT_CHANNELS);
    if channels == 0 {
        return Err(refused(
            "shake_no_channels",
            "`channels` is 0: no camera can hear it",
            "omit channels for the default channel (1), or set the sensor's bits",
        ));
    }
    let origin = origin(params)?;
    let radius = positive(params, "falloff_radius_m", false)?;
    let travel = positive(params, "travel_speed_m_s", false)?;
    let emit = ShakeEmit {
        origin,
        channels,
        preset,
        falloff_radius_m: radius.unwrap_or(f32::INFINITY),
        travel_speed_m_s: travel,
    };

    // Who will hear it, measured now (the emit is read in the next frame's PostUpdate).
    let driven = driven_camera(world);
    let mut listeners = Vec::new();
    for entity in cameras(world) {
        let (Some(transform), rig) = (
            world.get::<Transform>(entity),
            world.get::<CameraRig>(entity).is_some(),
        ) else {
            continue;
        };
        let Some(sensor) = effective_sensor(
            world.get::<ShakeSensor>(entity),
            rig,
            is_shot_driven(world, entity, driven),
        ) else {
            continue;
        };
        if !sensor.listens(channels) {
            continue;
        }
        let distance = origin.map_or(0.0, |o| (transform.translation - o).length());
        let shakes: Vec<Value> = emit
            .plan(distance, field.next_seed())
            .iter()
            .map(|planned| {
                let mut entry = kind_json(&planned.kind);
                if let Value::Object(map) = &mut entry {
                    map.insert("delay_s".into(), json!(planned.delay_s));
                    map.insert("delay_steps".into(), json!(field.delay_steps(planned.delay_s)));
                }
                entry
            })
            .collect();
        listeners.push(json!({
            "entity": entity.to_bits(),
            "distance_m": distance,
            "gain": sensor.effective_gain(),
            "shakes": shakes,
        }));
    }
    let listening = listeners.len();
    listeners.truncate(MAX_LISTED);
    if world.write_message(emit).is_none() {
        return Err(unsupported());
    }
    let kind = match preset {
        ShakePreset::Noise(_) => "noise",
        ShakePreset::Blast(_) => "blast",
        ShakePreset::Ground(_) => "ground",
    };
    let noise: Option<Shake> = match preset {
        ShakePreset::Noise(shake) => Some(shake),
        _ => None,
    };
    Ok(json!({
        "emitted": true,
        "preset": name,
        "kind": kind,
        "shake": noise.map(|s| json!({
            "amplitude": s.amplitude,
            "frequency_hz": s.frequency_hz,
            "duration_s": s.duration_secs,
            "falloff": s.falloff,
        })),
        "channels": channels,
        "origin": origin.map(vec3_json),
        "falloff_radius_m": radius,
        "travel_speed_m_s": travel,
        "fixed_hz": field.fixed_hz(),
        "listening": listening,
        "listeners": listeners,
        "presets": NAMED_SHAKES.iter().copied().chain(["blast", "ground"]).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_framework::camera::{CameraMode, CameraPlugin, Free};

    use super::*;

    fn app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )));
        let rig = app
            .world_mut()
            .spawn((
                Transform::default(),
                CameraRig {
                    mode: CameraMode::Free(Free::default()),
                    ..CameraRig::default()
                },
            ))
            .id();
        app.update();
        (app, rig)
    }

    fn reason(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    fn translation(app: &App, entity: Entity) -> Vec3 {
        app.world()
            .get::<Transform>(entity)
            .expect("transform")
            .translation
    }

    #[test]
    fn without_the_camera_plugin_every_method_is_unsupported() {
        let mut empty = App::new();
        for error in [
            camera_shake(empty.world_mut(), &json!({})).expect_err("shake"),
            camera_shake_sensor(empty.world_mut(), &json!({})).expect_err("read"),
            camera_set_shake_sensor(empty.world_mut(), &json!({ "entity": 1 })).expect_err("write"),
        ] {
            assert_eq!(error.code, protocol::UNSUPPORTED);
        }
    }

    #[test]
    fn a_plain_shake_reaches_the_rig_and_moves_it() {
        let (mut app, rig) = app();
        let out =
            camera_shake(app.world_mut(), &json!({ "preset": "impact", "amplitude": 0.4 })).expect("shake");
        assert_eq!(out["emitted"], true);
        assert_eq!(out["kind"], "noise");
        assert_eq!(out["listening"], 1);
        assert_eq!(out["listeners"][0]["entity"], rig.to_bits());
        assert_eq!(out["listeners"][0]["shakes"][0]["delay_steps"], 0);
        let mut moved = 0.0f32;
        for _ in 0..10 {
            app.update();
            moved = moved.max(translation(&app, rig).length());
        }
        assert!(moved > 0.01, "the rig shook: {moved}");
        let state = camera_shake_sensor(app.world_mut(), &json!({ "entity": rig.to_bits() })).expect("read");
        assert_eq!(state["camera"]["rig"], true);
        assert_eq!(state["camera"]["explicit"], false);
        assert_eq!(state["camera"]["sensor"]["channels"], 1);
        assert_eq!(state["camera"]["receiver"]["fired"], 1);
        assert!(state["field"]["emitted"].as_u64() >= Some(1));
    }

    #[test]
    fn a_travelling_blast_reports_its_delay_in_steps() {
        let (mut app, rig) = app();
        let out = camera_shake(
            app.world_mut(),
            &json!({ "preset": "blast", "origin": [0.0, 0.0, -686.0] }),
        )
        .expect("blast");
        let shake = &out["listeners"][0]["shakes"][0];
        assert_eq!(shake["delay_s"], 2.0);
        assert_eq!(shake["delay_steps"], 120);
        app.update();
        let state = camera_shake_sensor(app.world_mut(), &json!({})).expect("list");
        assert_eq!(state["count"], 1);
        assert_eq!(state["cameras"][0]["entity"], rig.to_bits());
        let pending = &state["cameras"][0]["receiver"]["pending"][0];
        assert_eq!(pending["frames_left"], 120);
        assert_eq!(pending["kind"], "noise");
    }

    #[test]
    fn the_sensor_can_be_set_read_and_removed() {
        let (mut app, rig) = app();
        let set = camera_set_shake_sensor(
            app.world_mut(),
            &json!({ "entity": rig.to_bits(), "channels": 6, "gain": 0.5, "combine": "max", "axes": "camera", "clock": "unscaled" }),
        )
        .expect("set");
        assert_eq!(set["before"]["channels"], 1, "the rig's default sensor");
        assert_eq!(set["after"]["channels"], 6);
        assert_eq!(set["after"]["combine"], "max");
        assert_eq!(set["after"]["axes"], "camera");
        assert_eq!(set["after"]["clock"], "unscaled");
        assert_eq!(set["explicit"], true);
        let sensor = *app.world().get::<ShakeSensor>(rig).expect("sensor");
        assert_eq!(sensor.channels, 6);
        assert_eq!(sensor.gain, 0.5);

        // The default channel no longer reaches it.
        let out = camera_shake(app.world_mut(), &json!({})).expect("shake");
        assert_eq!(out["listening"], 0);
        let out = camera_shake(app.world_mut(), &json!({ "channels": 4 })).expect("shake");
        assert_eq!(out["listening"], 1);

        let removed = camera_set_shake_sensor(
            app.world_mut(),
            &json!({ "entity": rig.to_bits(), "remove": true }),
        )
        .expect("remove");
        assert_eq!(removed["explicit"], false);
        assert_eq!(removed["after"]["channels"], 1, "back to the rig default");
        assert!(app.world().get::<ShakeSensor>(rig).is_none());

        // A plain entity with a transform can be given a sensor; it had none before.
        let plain = app.world_mut().spawn(Transform::default()).id();
        let set = camera_set_shake_sensor(
            app.world_mut(),
            &json!({ "entity": plain.to_bits(), "gain": 2.0 }),
        )
        .expect("plain");
        assert_eq!(set["before"], Value::Null);
        assert_eq!(set["after"]["gain"], 2.0);
    }

    #[test]
    fn mistakes_carry_reasons() {
        let (mut app, rig) = app();
        let entity = rig.to_bits();
        let cases = [
            (
                camera_shake(app.world_mut(), &json!({ "preset": "earthquake" })),
                "shake_unknown_preset",
            ),
            (
                camera_shake(app.world_mut(), &json!({ "preset": "blast", "amplitude": 1.0 })),
                "shake_override_not_noise",
            ),
            (
                camera_shake(app.world_mut(), &json!({ "channels": 0 })),
                "shake_no_channels",
            ),
            (
                camera_set_shake_sensor(app.world_mut(), &json!({ "entity": entity, "combine": "mul" })),
                "shake_unknown_combine",
            ),
            (
                camera_set_shake_sensor(app.world_mut(), &json!({ "entity": entity, "axes": "up" })),
                "shake_unknown_axes",
            ),
            (
                camera_set_shake_sensor(app.world_mut(), &json!({ "entity": entity, "clock": "wall" })),
                "shake_unknown_clock",
            ),
        ];
        for (result, expected) in cases {
            let error = result.expect_err(expected);
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{expected}");
            assert_eq!(reason(&error).as_deref(), Some(expected));
        }
        let bare = app.world_mut().spawn_empty().id();
        let error =
            camera_set_shake_sensor(app.world_mut(), &json!({ "entity": bare.to_bits() })).expect_err("bare");
        assert_eq!(reason(&error).as_deref(), Some("shake_not_a_camera"));
    }

    #[test]
    fn bad_params_are_invalid() {
        let (mut app, rig) = app();
        let entity = rig.to_bits();
        for bad in [
            json!({ "amplitude": -1.0 }),
            json!({ "duration_s": 0.0 }),
            json!({ "origin": [0.0, 1.0] }),
            json!({ "channels": -1 }),
            json!({ "channels": 4294967296u64 }),
            json!({ "falloff_radius_m": 0.0 }),
            json!({ "travel_speed_m_s": -3.0 }),
            json!({ "preset": 3 }),
        ] {
            let error = camera_shake(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{bad}");
        }
        for bad in [
            json!({}),
            json!({ "entity": entity, "gain": 200.0 }),
            json!({ "entity": entity, "remove": "yes" }),
            json!({ "entity": entity, "remove": true, "gain": 1.0 }),
        ] {
            let error = camera_set_shake_sensor(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(error.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let gone = app.world_mut().spawn_empty().id();
        app.world_mut().despawn(gone);
        for error in [
            camera_set_shake_sensor(app.world_mut(), &json!({ "entity": gone.to_bits() })).expect_err("gone"),
            camera_shake_sensor(app.world_mut(), &json!({ "entity": gone.to_bits() })).expect_err("gone"),
        ] {
            assert_eq!(error.code, protocol::NOT_FOUND);
        }
    }
}
