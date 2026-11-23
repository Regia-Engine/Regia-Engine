// SPDX-License-Identifier: MPL-2.0
//! `render.sequence` and `camera.set_path`: author, play, seek and query camera paths and shot
//! sequences (`regia_framework::shots`) from the agent protocol (book `agents/camera-paths.md`,
//! guide topic `camera_paths`).
//!
//! Both work headless: the sequence is sampled from a time value, so a seek followed by a
//! screenshot needs no stepping, and a played sequence advances on the engine's gameplay time
//! (`play.pause`, `play.step` and the time scale all apply). Calls land at a frame boundary and
//! the camera is moved later in that frame, so read the result of a seek from the next call.

use regia_app::prelude::*;
use regia_framework::shots::{
    CameraPath, DepthOfFieldTracks, RotationKey, Shot, ShotCamera, ShotPlayer, ShotSample, ShotSequence,
    Track,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most points `camera.set_path` returns in `preview`.
const MAX_PREVIEW: usize = 64;

/// Most shots in a sequence the agent protocol accepts.
const MAX_SHOTS: usize = 256;
/// Most keys in one camera path or property track.
const MAX_KEYS: usize = 4096;

/// Refuses a sequence with more shots or keys than the protocol keeps (memory stays bounded).
fn check_sizes(sequence: &ShotSequence) -> Result<(), RpcError> {
    if sequence.shots.len() > MAX_SHOTS {
        return Err(RpcError::invalid_params(format!(
            "a sequence has at most {MAX_SHOTS} shots, this one has {}",
            sequence.shots.len()
        )));
    }
    for shot in &sequence.shots {
        let camera = &shot.camera;
        let most = camera
            .path
            .keys
            .len()
            .max(camera.fov.0.len())
            .max(camera.roll.0.len())
            .max(camera.rotation.len());
        if most > MAX_KEYS {
            return Err(RpcError::invalid_params(format!(
                "shot `{}` has {most} keys on one track; the limit is {MAX_KEYS}",
                shot.name
            )));
        }
    }
    Ok(())
}

/// Reads an optional typed parameter; `null` and absent are both `None`.
fn parse<T: DeserializeOwned>(params: &Value, field: &str) -> Result<Option<T>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| RpcError::invalid_params(format!("`{field}`: {e}"))),
    }
}

fn flag(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a boolean"))),
    }
}

/// An optional finite number `>= min` (`> 0` when `strict`).
fn seconds(params: &Value, field: &str, strict: bool) -> Result<Option<f64>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && if strict { *f > 0.0 } else { *f >= 0.0 })
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`{field}` must be a number {} zero",
                    if strict { "greater than" } else { "of at least" }
                ))
            }),
    }
}

fn entity_param(params: &Value, field: &str) -> Result<Option<Entity>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => {
            let bits = v
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be an entity id")))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        }
    }
}

/// `time` (seconds) or `frame` (at the sequence's `fps`), never both.
fn time_param(params: &Value, fps: f32) -> Result<Option<f64>, RpcError> {
    let time = seconds(params, "time", false)?;
    let frame = match params.get("frame").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| RpcError::invalid_params("`frame` must be a whole number of at least zero"))?,
        ),
    };
    match (time, frame) {
        (Some(_), Some(_)) => Err(RpcError::invalid_params("give `time` or `frame`, not both")),
        (Some(t), None) => Ok(Some(t)),
        (None, Some(f)) => Ok(Some(regia_framework::shots::time_of_frame(f, fps))),
        (None, None) => Ok(None),
    }
}

fn sample_json(sequence: &ShotSequence, s: &ShotSample) -> Value {
    json!({
        "time": s.time,
        "shot": s.shot,
        "shot_name": sequence.shots[s.shot].name,
        "local_time": s.local_time,
        "eye": s.eye.to_array(),
        "target": s.target.to_array(),
        "up": s.up.to_array(),
        "rotation": s.rotation.to_array(),
        "roll_degrees": s.roll_degrees,
        "fov_y_degrees": s.fov_y_degrees,
        "dof": s.dof.map(|d| json!({
            "focus_distance": d.focus_distance,
            "aperture": d.aperture,
            "bokeh": d.bokeh,
        })),
        "fade": s.fade,
        "fade_color": s.fade_color,
        "finished": s.finished,
    })
}

/// The player's state: the loaded sequence's outline, the clock and the current sample.
fn state_json(player: &ShotPlayer) -> Value {
    let camera = player.camera().map(Entity::to_bits);
    let Some(sequence) = player.sequence() else {
        return json!({
            "loaded": false,
            "playing": false,
            "time": 0.0,
            "cuts": player.cuts(),
            "camera": camera,
        });
    };
    let mut start = 0.0f64;
    let shots: Vec<Value> = sequence
        .shots
        .iter()
        .map(|shot| {
            let outline = json!({
                "name": shot.name,
                "start": start,
                "duration": shot.duration,
                "keys": shot.camera.path.keys.len(),
            });
            start += f64::from(shot.duration);
            outline
        })
        .collect();
    let sample = player.sample();
    json!({
        "loaded": true,
        "name": sequence.name,
        "fps": sequence.fps,
        "duration": sequence.duration(),
        "shots": shots,
        "time": player.time(),
        "frame": (player.time() * f64::from(sequence.fps) + 1.0e-6).floor() as u64,
        "playing": player.is_playing(),
        "looping": player.is_looping(),
        "finished": player.is_finished(),
        "shot": sample.map(|s| s.shot),
        "shot_name": sample.map(|s| sequence.shots[s.shot].name.clone()),
        "local_time": sample.map(|s| s.local_time),
        "cuts": player.cuts(),
        "camera": camera,
        "sample": sample.map(|s| sample_json(sequence, s)),
    })
}

fn no_sequence() -> RpcError {
    RpcError::not_found(
        "no shot sequence is loaded",
        "render.sequence { \"action\": \"load\", \"ron\": \"...\" } or camera.set_path creates one",
    )
}

/// A sequence from the `ron` text or the `sequence` object of a request: parsed, migrated and
/// validated.
fn sequence_from(params: &Value) -> Result<ShotSequence, RpcError> {
    let sequence = sequence_from_unchecked(params)?;
    check_sizes(&sequence)?;
    Ok(sequence)
}

fn sequence_from_unchecked(params: &Value) -> Result<ShotSequence, RpcError> {
    let ron = params.get("ron").filter(|v| !v.is_null());
    let object = params.get("sequence").filter(|v| !v.is_null());
    match (ron, object) {
        (Some(_), Some(_)) => Err(RpcError::invalid_params("give `ron` or `sequence`, not both")),
        (None, None) => Err(RpcError::invalid_params(
            "`load` needs the sequence as `ron` (the .shots.ron text) or `sequence` (the same data as JSON)",
        )),
        (Some(text), None) => {
            let text = text
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`ron` must be a string"))?;
            ShotSequence::from_ron(text).map_err(|e| RpcError::invalid_params(e.to_string()))
        }
        (None, Some(value)) => {
            let mut sequence: ShotSequence = serde_json::from_value(value.clone())
                .map_err(|e| RpcError::invalid_params(format!("`sequence`: {e}")))?;
            sequence
                .migrate_to_latest()
                .and_then(|_| sequence.validate())
                .map_err(|e| RpcError::invalid_params(e.to_string()))?;
            Ok(sequence)
        }
    }
}

/// `render.sequence { action?, ... }`: load, play, pause, stop, seek, sample, query, export or
/// clear the shot sequence.
///
/// `action` defaults to `query`. `load` takes `ron` or `sequence` (plus `play`, `loop`, `camera`);
/// `play` takes `loop`, `camera` and an optional start (`time` or `frame`); `seek` and `sample`
/// take `time` (seconds) or `frame`.
pub(crate) fn render_sequence(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = match params.get("action").filter(|v| !v.is_null()) {
        None => "query",
        Some(v) => v
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("`action` must be a string"))?,
    };
    // Validate everything before touching the world.
    let camera = entity_param(params, "camera")?;
    let looping = flag(params, "loop")?;
    let play_now = flag(params, "play")?;

    world.init_resource::<ShotPlayer>();
    let mut player = world.resource_mut::<ShotPlayer>();
    let mut extra = Value::Null;
    match action {
        "query" => {}
        "load" => {
            let sequence = sequence_from(params)?;
            player.load(sequence);
            if camera.is_some() {
                player.set_camera(camera);
            }
            if let Some(looping) = looping {
                player.set_looping(looping);
            }
            if play_now == Some(true) {
                player.play();
            }
        }
        "play" => {
            let fps = player.sequence().ok_or_else(no_sequence)?.fps;
            let from = time_param(params, fps)?;
            if camera.is_some() {
                player.set_camera(camera);
            }
            if let Some(looping) = looping {
                player.set_looping(looping);
            }
            if let Some(from) = from {
                player.seek(from);
            }
            player.play();
        }
        "pause" => player.pause(),
        "stop" => player.stop(),
        "seek" => {
            let fps = player.sequence().ok_or_else(no_sequence)?.fps;
            let time = time_param(params, fps)?
                .ok_or_else(|| RpcError::invalid_params("`seek` needs `time` (seconds) or `frame`"))?;
            player.seek(time);
        }
        "sample" => {
            let sequence = player.sequence().ok_or_else(no_sequence)?;
            let time = time_param(params, sequence.fps)?
                .ok_or_else(|| RpcError::invalid_params("`sample` needs `time` (seconds) or `frame`"))?;
            extra = match sequence.sample(time) {
                Some(s) => sample_json(sequence, &s),
                None => Value::Null,
            };
        }
        "export" => {
            let sequence = player.sequence().ok_or_else(no_sequence)?;
            extra = json!(sequence.to_ron().map_err(|e| RpcError::internal(e.to_string()))?);
        }
        "clear" => player.clear(),
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown action `{other}` (load, play, pause, stop, seek, sample, query, export, clear)"
            )));
        }
    }
    let mut out = json!({ "action": action, "state": state_json(&player) });
    match action {
        "sample" => out["sample"] = extra,
        "export" => out["ron"] = extra,
        _ => {}
    }
    out["fade_layer"] = fade_layer_json(world);
    Ok(out)
}

/// What the compositor's fade layer is doing: `installed` once a sequence has shown a fade,
/// `amount` the opacity it draws (it follows the sample from the next frame on).
fn fade_layer_json(world: &World) -> Value {
    match world.get_resource::<regia_framework::shots::ShotFadeLayer>() {
        Some(layer) => {
            let state = layer.0.get();
            json!({ "installed": true, "amount": state.amount, "color": state.color })
        }
        None => json!({ "installed": false, "amount": 0.0, "color": [0.0, 0.0, 0.0] }),
    }
}

/// The preview points of a shot: `count` evenly spaced samples from its start to its end.
fn preview(shot: &Shot, count: usize) -> Vec<Value> {
    if count == 0 {
        return Vec::new();
    }
    // A one-shot sequence holds the last frame at the end, so the final point is the shot's end.
    let alone = ShotSequence {
        shots: vec![shot.clone()],
        ..ShotSequence::default()
    };
    (0..count)
        .filter_map(|i| {
            let fraction = if count == 1 {
                0.0
            } else {
                i as f64 / (count - 1) as f64
            };
            let time = f64::from(shot.duration) * fraction;
            let s = alone.sample(time)?;
            Some(json!({
                "time": time,
                "eye": s.eye.to_array(),
                "target": s.target.to_array(),
                "fov_y_degrees": s.fov_y_degrees,
            }))
        })
        .collect()
}

/// `camera.set_path { path, shot?, name?, duration?, fov?, roll?, rotation?, dof?, fade_in?, fade_out?,
/// camera?, play?, preview?, dry_run? }`.
///
/// With `shot` naming a shot of the loaded sequence, replaces that shot's camera path (and any of
/// `duration`, `fov`, `roll`, `rotation` (`null` clears it), `dof`, `fade_in`, `fade_out` that are given); the clock and play
/// state are kept. Without `shot`, creates a one-shot sequence from the path (duration defaults to
/// the last key's time) and loads it, replacing the loaded one. `dry_run` validates and previews
/// without applying.
pub(crate) fn camera_set_path(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dry_run = flag(params, "dry_run")?.unwrap_or(false);
    let play = flag(params, "play")?.unwrap_or(false);
    let camera = entity_param(params, "camera")?;
    let path: CameraPath = parse(params, "path")?.ok_or_else(|| {
        RpcError::invalid_params(
            "`path` is required: { keys: [{ time, position: [x, y, z] }], interp?, look_at? }",
        )
    })?;
    let duration = seconds(params, "duration", true)?.map(|d| d as f32);
    let fade_in = seconds(params, "fade_in", false)?.map(|d| d as f32);
    let fade_out = seconds(params, "fade_out", false)?.map(|d| d as f32);
    let fov: Option<Track> = parse(params, "fov")?;
    let roll: Option<Track> = parse(params, "roll")?;
    let rotation: Option<Vec<RotationKey>> = parse(params, "rotation")?;
    let clear_rotation = params.get("rotation").is_some_and(Value::is_null);
    let dof: Option<DepthOfFieldTracks> = parse(params, "dof")?;
    let remove_dof = params.get("dof").is_some_and(Value::is_null);
    let preview_count = match params.get("preview").filter(|v| !v.is_null()) {
        None => 0,
        Some(v) => v.as_u64().filter(|n| *n <= MAX_PREVIEW as u64).ok_or_else(|| {
            RpcError::invalid_params(format!("`preview` must be a whole number up to {MAX_PREVIEW}"))
        })? as usize,
    };
    let shot_name = match params.get("shot").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params("`shot` must be a shot name"))?
                .to_string(),
        ),
    };

    world.init_resource::<ShotPlayer>();
    let loaded = world.resource::<ShotPlayer>().sequence().cloned();
    let invalid = |e: regia_framework::shots::ShotSequenceError| RpcError::invalid_params(e.to_string());

    let (mut sequence, index, created) = match (shot_name, loaded) {
        (Some(name), Some(sequence)) => {
            let index = sequence.shot_index(&name).ok_or_else(|| {
                let names: Vec<&str> = sequence.shots.iter().map(|s| s.name.as_str()).collect();
                RpcError::not_found(
                    format!("no shot named `{name}`"),
                    format!("the loaded sequence has: {}", names.join(", ")),
                )
            })?;
            (sequence, index, false)
        }
        (Some(_), None) => return Err(no_sequence()),
        (None, _) => {
            let name = match params.get("name").filter(|v| !v.is_null()) {
                None => "adhoc".to_string(),
                Some(v) => v
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| RpcError::invalid_params("`name` must be a non-empty string"))?
                    .to_string(),
            };
            let duration = match duration {
                Some(d) => d,
                None => path
                    .keys
                    .last()
                    .map(|k| k.time)
                    .filter(|t| *t > 0.0)
                    .ok_or_else(|| {
                        RpcError::invalid_params("give `duration`: the path's last key is at time zero")
                    })?,
            };
            let shot = Shot {
                name: name.clone(),
                duration,
                camera: ShotCamera {
                    path: path.clone(),
                    fov: Track::default(),
                    roll: Track::default(),
                    dof: None,
                    rotation: Vec::new(),
                },
                fade_in: 0.0,
                fade_out: 0.0,
                fade_color: [0.0; 3],
            };
            let sequence = ShotSequence {
                name,
                shots: vec![shot],
                ..ShotSequence::default()
            };
            (sequence, 0, true)
        }
    };

    let shot = &mut sequence.shots[index];
    shot.camera.path = path;
    if !created && let Some(d) = duration {
        shot.duration = d;
    }
    if let Some(fov) = fov {
        shot.camera.fov = fov;
    }
    if let Some(roll) = roll {
        shot.camera.roll = roll;
    }
    if dof.is_some() || remove_dof {
        shot.camera.dof = dof;
    }
    if let Some(keys) = rotation {
        shot.camera.rotation = keys;
    } else if clear_rotation {
        shot.camera.rotation.clear();
    }
    if let Some(f) = fade_in {
        shot.fade_in = f;
    }
    if let Some(f) = fade_out {
        shot.fade_out = f;
    }
    sequence.validate().map_err(invalid)?;
    check_sizes(&sequence)?;

    let shot = &sequence.shots[index];
    let points = preview(shot, preview_count);
    let mut out = json!({
        "shot": shot.name,
        "index": index,
        "created": created,
        "applied": !dry_run,
        "duration": shot.duration,
        "keys": shot.camera.path.keys.len(),
        "preview": points,
    });
    if !dry_run {
        let start = sequence.shot_start(index);
        let mut player = world.resource_mut::<ShotPlayer>();
        if created {
            player.load(sequence);
        } else {
            player.replace_sequence(sequence);
        }
        if camera.is_some() {
            player.set_camera(camera);
        }
        if play {
            if !created {
                player.seek(start);
            }
            player.play();
        }
        out["state"] = state_json(&player);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RON: &str = r#"(
        version: 1,
        name: "demo",
        shots: [
            (name: "a", duration: 2.0,
             camera: (path: (interp: Linear,
                             keys: [(time: 0.0, position: (0.0, 0.0, 0.0)), (time: 2.0, position: (4.0, 0.0, 0.0))],
                             look_at: Point((0.0, 0.0, -5.0))))),
            (name: "b", duration: 3.0,
             camera: (path: (interp: Linear,
                             keys: [(time: 0.0, position: (0.0, 5.0, 0.0)), (time: 3.0, position: (0.0, 5.0, 6.0))],
                             look_at: Heading)),
             fade_out: 1.0),
        ],
    )"#;

    fn world_with_sequence() -> World {
        let mut world = World::new();
        let out = render_sequence(&mut world, &json!({ "action": "load", "ron": RON })).expect("load");
        assert_eq!(out["state"]["loaded"], true);
        world
    }

    #[test]
    fn query_on_an_empty_app_is_a_valid_empty_state() {
        let mut world = World::new();
        let out = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(out["action"], "query");
        assert_eq!(out["state"]["loaded"], false);
        assert!(render_sequence(&mut world, &json!({ "action": "seek", "time": 1.0 })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": "play" })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": "export" })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": "dance" })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": 3 })).is_err());
    }

    #[test]
    fn load_reports_the_outline_and_the_first_sample() {
        let mut world = world_with_sequence();
        let out = render_sequence(&mut world, &json!({})).expect("query");
        let state = &out["state"];
        assert_eq!(state["name"], "demo");
        assert_eq!(state["duration"], 5.0);
        assert_eq!(state["shots"][1]["name"], "b");
        assert_eq!(state["shots"][1]["start"], 2.0);
        assert_eq!(state["playing"], false);
        assert_eq!(state["cuts"], 1);
        assert_eq!(state["sample"]["shot_name"], "a");
        assert_eq!(state["sample"]["eye"], json!([0.0, 0.0, 0.0]));
    }

    #[test]
    fn seek_by_time_and_by_frame_land_on_the_same_sample() {
        let mut world = world_with_sequence();
        let by_time = render_sequence(&mut world, &json!({ "action": "seek", "time": 1.0 })).expect("seek");
        assert_eq!(by_time["state"]["sample"]["eye"], json!([2.0, 0.0, 0.0]));
        assert_eq!(by_time["state"]["frame"], 30);
        let by_frame = render_sequence(&mut world, &json!({ "action": "seek", "frame": 30 })).expect("seek");
        assert_eq!(by_frame["state"]["sample"], by_time["state"]["sample"]);
        assert!(render_sequence(&mut world, &json!({ "action": "seek" })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": "seek", "time": 1, "frame": 3 })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": "seek", "time": -1 })).is_err());
        assert!(render_sequence(&mut world, &json!({ "action": "seek", "frame": 1.5 })).is_err());
    }

    #[test]
    fn a_cut_shows_at_the_boundary_and_sample_does_not_move_the_clock() {
        let mut world = world_with_sequence();
        let before =
            render_sequence(&mut world, &json!({ "action": "sample", "time": 1.999 })).expect("sample");
        let after = render_sequence(&mut world, &json!({ "action": "sample", "time": 2.0 })).expect("sample");
        assert_eq!(before["sample"]["shot_name"], "a");
        assert_eq!(after["sample"]["shot_name"], "b");
        assert_eq!(after["sample"]["eye"], json!([0.0, 5.0, 0.0]));
        assert_eq!(after["state"]["time"], 0.0, "the clock stayed at zero");
        let end = render_sequence(&mut world, &json!({ "action": "sample", "time": 99.0 })).expect("sample");
        assert_eq!(end["sample"]["finished"], true);
        assert_eq!(end["sample"]["fade"], 1.0, "the last shot fades out at its end");
    }

    #[test]
    fn the_fade_layer_report_is_off_until_a_fade_is_drawn() {
        let mut world = world_with_sequence();
        let out = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(out["fade_layer"]["installed"], false);
        assert_eq!(out["fade_layer"]["amount"], 0.0);
        let handle = {
            let mut overlay = regia_render::WindowOverlay::default();
            regia_render::install_fade(&mut overlay)
        };
        handle.set(regia_render::FadeState {
            amount: 0.5,
            color: [0.0, 0.0, 1.0],
        });
        world.insert_resource(regia_framework::shots::ShotFadeLayer(handle));
        let out = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(out["fade_layer"]["installed"], true);
        assert_eq!(out["fade_layer"]["amount"], 0.5);
        assert_eq!(out["fade_layer"]["color"], json!([0.0, 0.0, 1.0]));
    }

    #[test]
    fn play_pause_stop_and_loop_set_the_flags() {
        let mut world = world_with_sequence();
        let out = render_sequence(
            &mut world,
            &json!({ "action": "play", "time": 1.0, "loop": true }),
        )
        .expect("play");
        assert_eq!(out["state"]["playing"], true);
        assert_eq!(out["state"]["looping"], true);
        assert_eq!(out["state"]["time"], 1.0);
        let out = render_sequence(&mut world, &json!({ "action": "pause" })).expect("pause");
        assert_eq!(out["state"]["playing"], false);
        assert_eq!(out["state"]["time"], 1.0);
        let out = render_sequence(&mut world, &json!({ "action": "stop" })).expect("stop");
        assert_eq!(out["state"]["time"], 0.0);
        // The player advances on game time: drive it directly the way the system does.
        world.resource_mut::<ShotPlayer>().play();
        world.resource_mut::<ShotPlayer>().advance(2.5);
        let out = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(out["state"]["shot_name"], "b");
        assert_eq!(out["state"]["cuts"], 2, "the load, then the boundary into shot b");
    }

    #[test]
    fn load_accepts_json_and_rejects_bad_input() {
        let mut world = World::new();
        let sequence = ShotSequence::from_ron(RON).expect("ron");
        let value = serde_json::to_value(&sequence).expect("json");
        let out = render_sequence(
            &mut world,
            &json!({ "action": "load", "sequence": value, "play": true }),
        )
        .expect("load json");
        assert_eq!(out["state"]["playing"], true);
        assert_eq!(out["state"]["duration"], 5.0);

        // Hand-written JSON uses the externally tagged enum spelling.
        let hand = json!({
            "version": 1,
            "shots": [{
                "name": "s", "duration": 1.0,
                "camera": { "path": { "keys": [{ "time": 0.0, "position": [0, 1, 2] }], "look_at": { "Point": [0, 0, 0] } } }
            }]
        });
        assert!(render_sequence(&mut world, &json!({ "action": "load", "sequence": hand })).is_ok());

        for bad in [
            json!({ "action": "load" }),
            json!({ "action": "load", "ron": 5 }),
            json!({ "action": "load", "ron": "(version: 1, shots: [])" }),
            json!({ "action": "load", "ron": "(version: 9, shots: [])" }),
            json!({ "action": "load", "ron": "nonsense" }),
            json!({ "action": "load", "ron": RON, "sequence": {} }),
            json!({ "action": "load", "sequence": { "version": 1, "shots": 5 } }),
            json!({ "action": "load", "ron": RON, "camera": "x" }),
            json!({ "action": "load", "ron": RON, "loop": "yes" }),
        ] {
            assert!(render_sequence(&mut world, &bad).is_err(), "{bad} should fail");
        }
        assert_eq!(
            render_sequence(&mut world, &json!({})).expect("state")["state"]["shots"][0]["name"],
            "s",
            "a failed load keeps the previous sequence"
        );
    }

    #[test]
    fn export_returns_canonical_text_that_loads_back() {
        let mut world = world_with_sequence();
        let out = render_sequence(&mut world, &json!({ "action": "export" })).expect("export");
        let text = out["ron"].as_str().expect("ron").to_string();
        assert_eq!(
            ShotSequence::from_ron(&text).expect("parses"),
            ShotSequence::from_ron(RON).expect("ron")
        );
        let again = render_sequence(&mut world, &json!({ "action": "load", "ron": text })).expect("reload");
        assert_eq!(again["state"]["duration"], 5.0);
        let cleared = render_sequence(&mut world, &json!({ "action": "clear" })).expect("clear");
        assert_eq!(cleared["state"]["loaded"], false);
    }

    fn path_json() -> Value {
        json!({
            "interp": "CatmullRom",
            "keys": [
                { "time": 0.0, "position": [0, 2, 10] },
                { "time": 1.0, "position": [3, 2, 6] },
                { "time": 2.0, "position": [6, 2, 2] }
            ],
            "look_at": { "Point": [6, 1, 0] }
        })
    }

    #[test]
    fn oversized_sequences_are_refused() {
        let mut world = World::new();
        let keys: Vec<Value> = (0..=MAX_KEYS)
            .map(|i| json!({ "time": i as f64 * 0.01, "position": [0.0, 0.0, 0.0] }))
            .collect();
        let err = camera_set_path(
            &mut world,
            &json!({ "path": { "interp": "Linear", "keys": keys, "look_at": "Heading" }, "duration": 100.0 }),
        )
        .expect_err("too many keys");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
    }

    #[test]
    fn set_path_without_a_shot_creates_an_ad_hoc_sequence() {
        let mut world = World::new();
        let out = camera_set_path(
            &mut world,
            &json!({ "path": path_json(), "preview": 3, "fov": [{ "time": 0.0, "value": 30.0 }], "play": true }),
        )
        .expect("set_path");
        assert_eq!(out["created"], true);
        assert_eq!(out["shot"], "adhoc");
        assert_eq!(out["duration"], 2.0, "defaults to the last key's time");
        assert_eq!(out["keys"], 3);
        let points = out["preview"].as_array().expect("preview");
        assert_eq!(points.len(), 3);
        assert_eq!(points[0]["eye"], json!([0.0, 2.0, 10.0]));
        assert_eq!(
            points[2]["eye"],
            json!([6.0, 2.0, 2.0]),
            "the last point is the shot's end"
        );
        assert_eq!(points[2]["fov_y_degrees"], 30.0);
        assert_eq!(out["state"]["playing"], true);
        assert_eq!(out["state"]["shots"][0]["name"], "adhoc");
    }

    #[test]
    fn set_path_replaces_one_shot_and_keeps_the_clock() {
        let mut world = world_with_sequence();
        render_sequence(&mut world, &json!({ "action": "seek", "time": 3.0 })).expect("seek");
        let new_path = json!({
            "interp": "Linear",
            "keys": [{ "time": 0.0, "position": [9, 9, 9] }, { "time": 3.0, "position": [9, 9, 0] }],
            "look_at": "Heading"
        });
        let out = camera_set_path(
            &mut world,
            &json!({ "shot": "b", "path": new_path, "dof": { "focus_on_target": true }, "fade_in": 0.5 }),
        )
        .expect("set_path");
        assert_eq!(out["created"], false);
        assert_eq!(out["index"], 1);
        assert_eq!(out["state"]["time"], 3.0, "the clock was kept");
        let z = out["state"]["sample"]["eye"][2].as_f64().expect("z");
        assert!((z - 6.0).abs() < 1.0e-4, "a third of the way from 9 to 0: {z}");
        assert_eq!(
            out["state"]["sample"]["fade"]
                .as_f64()
                .map(|f| (f * 1000.0).round()),
            Some(0.0)
        );
        assert!(out["state"]["sample"]["dof"].is_object());
        // `dof: null` removes it again; the other shot is untouched.
        let out = camera_set_path(
            &mut world,
            &json!({ "shot": "b", "path": path_json(), "duration": 2.0, "dof": null }),
        )
        .expect("set_path");
        assert!(out["state"]["sample"]["dof"].is_null());
        assert_eq!(out["state"]["duration"], 4.0);
        assert_eq!(out["state"]["shots"][0]["name"], "a");
    }

    #[test]
    fn set_path_dry_run_validates_and_previews_without_applying() {
        let mut world = world_with_sequence();
        let out = camera_set_path(
            &mut world,
            &json!({ "shot": "a", "path": path_json(), "dry_run": true, "preview": 2 }),
        )
        .expect("dry run");
        assert_eq!(out["applied"], false);
        assert!(out.get("state").is_none());
        assert_eq!(out["preview"].as_array().map(Vec::len), Some(2));
        let state = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(
            state["state"]["sample"]["eye"],
            json!([0.0, 0.0, 0.0]),
            "unchanged"
        );
        assert_eq!(state["state"]["shots"][0]["keys"], 2);
    }

    #[test]
    fn set_path_rejects_bad_input_and_leaves_the_sequence_alone() {
        let mut world = world_with_sequence();
        let unsorted = json!({ "keys": [{ "time": 1.0, "position": [0, 0, 0] }, { "time": 0.5, "position": [1, 0, 0] }] });
        let at_zero = json!({ "keys": [{ "time": 0.0, "position": [0, 0, 0] }] });
        for bad in [
            json!({}),
            json!({ "path": 5 }),
            json!({ "path": { "keys": [] } }),
            json!({ "path": unsorted, "duration": 2.0 }),
            json!({ "path": at_zero }),
            json!({ "shot": "nope", "path": path_json() }),
            json!({ "path": path_json(), "duration": 0 }),
            json!({ "path": path_json(), "preview": 1000 }),
            json!({ "path": path_json(), "fov": [{ "time": 0.0, "value": 400.0 }] }),
            json!({ "path": path_json(), "name": "" }),
            json!({ "path": path_json(), "camera": "x" }),
            json!({ "path": path_json(), "dry_run": 1 }),
        ] {
            assert!(camera_set_path(&mut world, &bad).is_err(), "{bad} should fail");
        }
        let mut empty = World::new();
        assert!(camera_set_path(&mut empty, &json!({ "shot": "a", "path": path_json() })).is_err());
        let state = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(state["state"]["name"], "demo");
        assert_eq!(state["state"]["cuts"], 1);
    }

    /// ADR-8640 relaxed `.shots.ron` validation: a camera path key past the shot's end is a
    /// control key the clock never reaches (a shortened Catmull-Rom path keeps them). The agent
    /// call follows the file rule: the path is accepted, the shot keeps its duration, and the
    /// sequence's cuts and total length do not move.
    #[test]
    fn set_path_accepts_a_key_past_the_shot_end_and_keeps_the_shot_duration() {
        let mut world = world_with_sequence();
        let past_end = json!({ "keys": [{ "time": 0.0, "position": [0, 0, 0] }, { "time": 9.0, "position": [1, 0, 0] }] });
        let out = camera_set_path(&mut world, &json!({ "shot": "a", "path": past_end }))
            .expect("a control key past the end validates");
        assert_eq!(out["shot"], "a");
        assert_eq!(
            out["duration"], 2.0,
            "the shot keeps its duration: the key past it is never reached"
        );
        assert_eq!(out["keys"], 2);
        let state = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(state["state"]["duration"], 5.0);
        assert_eq!(state["state"]["shots"][1]["start"], 2.0);
        assert_eq!(state["state"]["cuts"], 1);
        // A negative key time is still outside the shot and still refused.
        let before_zero = json!({ "keys": [{ "time": -1.0, "position": [0, 0, 0] }, { "time": 1.0, "position": [1, 0, 0] }] });
        assert!(camera_set_path(&mut world, &json!({ "shot": "a", "path": before_zero })).is_err());
    }

    // ---- Error codes, idempotence and determinism (agent-tests lane) -----------------------

    fn code_of(result: Result<Value, RpcError>) -> i64 {
        result.expect_err("expected an error").code
    }

    #[test]
    fn sequence_actions_before_load_are_not_found_and_bad_params_are_invalid() {
        let mut world = World::new();
        for action in ["play", "seek", "sample", "export"] {
            let params = json!({ "action": action, "time": 1.0 });
            assert_eq!(
                code_of(render_sequence(&mut world, &params)),
                crate::protocol::NOT_FOUND,
                "{action} before load"
            );
        }
        // pause / stop / clear / query are harmless on an empty player.
        for action in ["pause", "stop", "clear", "query"] {
            let out = render_sequence(&mut world, &json!({ "action": action })).expect(action);
            assert_eq!(out["state"]["loaded"], false, "{action}");
        }
        assert_eq!(
            code_of(render_sequence(&mut world, &json!({ "action": "dance" }))),
            crate::protocol::INVALID_PARAMS
        );
        let null_action =
            render_sequence(&mut world, &json!({ "action": null })).expect("null action is query");
        assert_eq!(null_action["action"], "query");
    }

    #[test]
    fn sequence_time_and_frame_validation_table() {
        let mut world = world_with_sequence();
        for (params, why) in [
            (json!({ "action": "seek", "time": "1" }), "string time"),
            (json!({ "action": "seek", "frame": -1 }), "negative frame"),
            (json!({ "action": "seek", "frame": "3" }), "string frame"),
            (json!({ "action": "sample" }), "sample needs a time"),
            (
                json!({ "action": "sample", "time": 1, "frame": 2 }),
                "both time and frame",
            ),
            (json!({ "action": "play", "time": -0.1 }), "negative play start"),
            (
                json!({ "action": "query", "camera": "e" }),
                "camera must be numeric",
            ),
            (json!({ "action": "query", "loop": 1 }), "loop must be boolean"),
            (
                json!({ "action": "query", "play": "yes" }),
                "play must be boolean",
            ),
        ] {
            assert_eq!(
                code_of(render_sequence(&mut world, &params)),
                crate::protocol::INVALID_PARAMS,
                "{why}"
            );
        }
    }

    #[test]
    fn a_rejected_call_never_changes_the_player() {
        let mut world = world_with_sequence();
        render_sequence(&mut world, &json!({ "action": "seek", "time": 1.5 })).expect("seek");
        let before = render_sequence(&mut world, &json!({})).expect("query");
        for bad in [
            json!({ "action": "seek", "time": -1 }),
            json!({ "action": "load", "ron": "nonsense" }),
            json!({ "action": "play", "time": 1, "frame": 1 }),
            json!({ "action": "stop", "loop": "x" }),
        ] {
            assert!(render_sequence(&mut world, &bad).is_err());
        }
        assert_eq!(before, render_sequence(&mut world, &json!({})).expect("query"));
    }

    #[test]
    fn invalid_sequences_are_rejected_with_a_named_problem() {
        let mut world = World::new();
        let shot = |name: &str| {
            json!({
                "name": name, "duration": 1.0,
                "camera": { "path": { "keys": [{ "time": 0.0, "position": [0, 0, 0] }], "look_at": "Heading" } }
            })
        };
        let mut over_faded = shot("a");
        over_faded["fade_in"] = json!(0.8);
        over_faded["fade_out"] = json!(0.8);
        let mut bad_color = shot("a");
        bad_color["fade_color"] = json!([2.0, 0.0, 0.0]);
        let mut negative_fade = shot("a");
        negative_fade["fade_in"] = json!(-1.0);
        let mut zero_duration = shot("a");
        zero_duration["duration"] = json!(0.0);
        for (sequence, why) in [
            (
                json!({ "version": 1, "shots": [shot("a"), shot("a")] }),
                "duplicate names",
            ),
            (json!({ "version": 1, "shots": [shot("")] }), "empty name"),
            (
                json!({ "version": 1, "fps": 0.0, "shots": [shot("a")] }),
                "zero fps",
            ),
            (
                json!({ "version": 1, "fps": 5000.0, "shots": [shot("a")] }),
                "fps above 1000",
            ),
            (
                json!({ "version": 1, "shots": [over_faded] }),
                "fades longer than the shot",
            ),
            (
                json!({ "version": 1, "shots": [bad_color] }),
                "fade color out of range",
            ),
            (json!({ "version": 1, "shots": [negative_fade] }), "negative fade"),
            (json!({ "version": 1, "shots": [zero_duration] }), "zero duration"),
            (json!({ "version": 1, "shots": [] }), "no shots"),
            (json!({ "version": 99, "shots": [shot("a")] }), "future version"),
        ] {
            let err = render_sequence(&mut world, &json!({ "action": "load", "sequence": sequence }))
                .expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
        // Nothing was loaded by any of them.
        assert_eq!(
            render_sequence(&mut world, &json!({})).expect("query")["state"]["loaded"],
            false
        );
    }

    #[test]
    fn sequence_calls_are_deterministic_and_idempotent() {
        let run = || {
            let mut world = world_with_sequence();
            let mut outs = Vec::new();
            for t in [0.0, 0.3, 1.0, 1.999, 2.0, 4.5, 5.0, 99.0] {
                outs.push(
                    render_sequence(&mut world, &json!({ "action": "sample", "time": t })).expect("sample"),
                );
            }
            outs.push(render_sequence(&mut world, &json!({ "action": "export" })).expect("export"));
            outs
        };
        assert_eq!(run(), run(), "two fresh worlds answer identically");
        let mut world = world_with_sequence();
        let a = render_sequence(&mut world, &json!({ "action": "seek", "time": 2.5 })).expect("seek");
        let b = render_sequence(&mut world, &json!({ "action": "seek", "time": 2.5 })).expect("seek again");
        assert_eq!(a, b, "seeking twice to the same time is idempotent");
        let p1 = render_sequence(&mut world, &json!({ "action": "pause" })).expect("pause");
        let p2 = render_sequence(&mut world, &json!({ "action": "pause" })).expect("pause again");
        assert_eq!(p1, p2);
        let c1 = render_sequence(&mut world, &json!({ "action": "clear" })).expect("clear");
        let c2 = render_sequence(&mut world, &json!({ "action": "clear" })).expect("clear again");
        assert_eq!(c1, c2);
    }

    #[test]
    fn camera_binding_is_reported_and_kept() {
        let mut world = world_with_sequence();
        let camera = world.spawn_empty().id();
        let bits = camera.to_bits();
        let out = render_sequence(&mut world, &json!({ "action": "play", "camera": bits })).expect("play");
        assert_eq!(out["state"]["camera"], bits);
        // A later call without `camera` keeps the binding.
        let out = render_sequence(&mut world, &json!({ "action": "pause" })).expect("pause");
        assert_eq!(out["state"]["camera"], bits);
    }

    #[test]
    fn set_path_preview_bounds_and_shape() {
        let mut world = World::new();
        let none = camera_set_path(&mut world, &json!({ "path": path_json(), "dry_run": true }))
            .expect("no preview");
        assert_eq!(none["preview"], json!([]));
        let zero = camera_set_path(
            &mut world,
            &json!({ "path": path_json(), "dry_run": true, "preview": 0 }),
        )
        .expect("0");
        assert_eq!(zero["preview"], json!([]));
        let one = camera_set_path(
            &mut world,
            &json!({ "path": path_json(), "dry_run": true, "preview": 1 }),
        )
        .expect("1");
        assert_eq!(one["preview"].as_array().map(Vec::len), Some(1));
        assert_eq!(one["preview"][0]["time"], 0.0);
        let max = camera_set_path(
            &mut world,
            &json!({ "path": path_json(), "dry_run": true, "preview": MAX_PREVIEW }),
        )
        .expect("max");
        assert_eq!(max["preview"].as_array().map(Vec::len), Some(MAX_PREVIEW));
        for bad in [json!(MAX_PREVIEW + 1), json!(-1), json!("3"), json!(1.5)] {
            let err = camera_set_path(&mut world, &json!({ "path": path_json(), "preview": bad }))
                .expect_err("preview");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        }
        // A dry run on an empty app creates nothing.
        assert_eq!(
            render_sequence(&mut world, &json!({})).expect("query")["state"]["loaded"],
            false
        );
    }

    #[test]
    fn set_path_error_codes_and_track_limits() {
        let mut world = world_with_sequence();
        let missing =
            camera_set_path(&mut world, &json!({ "shot": "zzz", "path": path_json() })).expect_err("shot");
        assert_eq!(missing.code, crate::protocol::NOT_FOUND);
        let mut empty = World::new();
        let none = camera_set_path(&mut empty, &json!({ "shot": "a", "path": path_json() }))
            .expect_err("no sequence");
        assert_eq!(none.code, crate::protocol::NOT_FOUND);
        for (extra, why) in [
            (json!({ "shot": 5 }), "numeric shot"),
            (json!({ "duration": 1e300 }), "duration overflowing f32"),
            (json!({ "duration": -1 }), "negative duration"),
            (json!({ "duration": "2" }), "string duration"),
            (json!({ "fade_in": -0.1 }), "negative fade_in"),
            (
                json!({ "fade_in": 1.5, "fade_out": 1.5 }),
                "fades longer than the shot",
            ),
            (json!({ "fov": [{ "time": 0.0, "value": 0.5 }] }), "fov below 1"),
            (
                json!({ "fov": [{ "time": 0.0, "value": 30.0 }, { "time": 0.0, "value": 40.0 }] }),
                "fov keys not increasing",
            ),
            (
                json!({ "fov": [{ "time": 5.0, "value": 30.0 }] }),
                "fov key past the shot",
            ),
            (
                json!({ "roll": [{ "time": 0.0, "value": 99999.0 }] }),
                "roll out of range",
            ),
            (
                json!({ "dof": { "focus_distance": [{ "time": 0.0, "value": 0.0 }] } }),
                "focus distance below minimum",
            ),
            (json!({ "dof": 7 }), "dof of the wrong type"),
            (json!({ "name": 3 }), "numeric name"),
        ] {
            let mut params = json!({ "path": path_json() });
            params
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            let err = camera_set_path(&mut world, &params).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
        // Not one of them changed the loaded sequence.
        let state = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(state["state"]["name"], "demo");
        assert_eq!(state["state"]["duration"], 5.0);
    }

    #[test]
    fn set_path_replacement_is_idempotent_and_play_seeks_to_the_shot() {
        let mut world = world_with_sequence();
        let params = json!({ "shot": "b", "path": path_json(), "duration": 2.0 });
        let a = camera_set_path(&mut world, &params).expect("first");
        let b = camera_set_path(&mut world, &params).expect("second");
        assert_eq!(a, b, "applying the same replacement twice leaves the same state");
        let played = camera_set_path(
            &mut world,
            &json!({ "shot": "b", "path": path_json(), "duration": 2.0, "play": true }),
        )
        .expect("play");
        assert_eq!(played["state"]["playing"], true);
        assert_eq!(
            played["state"]["time"], 2.0,
            "play on a replaced shot starts at that shot"
        );
    }

    #[test]
    fn set_path_creation_is_deterministic_and_replaces_the_loaded_sequence() {
        let params = json!({ "path": path_json(), "name": "orbit", "preview": 5, "duration": 4.0 });
        let run = || camera_set_path(&mut World::new(), &params).expect("create");
        assert_eq!(run(), run());
        let mut world = world_with_sequence();
        let out = camera_set_path(&mut world, &params).expect("create over a loaded sequence");
        assert_eq!(out["created"], true);
        assert_eq!(out["duration"], 4.0);
        let state = render_sequence(&mut world, &json!({})).expect("query");
        assert_eq!(state["state"]["name"], "orbit");
        assert_eq!(state["state"]["shots"].as_array().map(Vec::len), Some(1));
    }
}
