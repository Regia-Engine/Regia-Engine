// SPDX-License-Identifier: MPL-2.0
//! `sequence.player`: load, play, pause, stop, seek, sample, bind, configure and report sequence
//! players (`regia_framework::sequencer`, ADR-1614; book `agents/sequence-player.md`, guide topic
//! `sequence_player`).
//!
//! A player is an entity with a `SequencePlayer`: a `.seq.ron` sequence and, optionally, a
//! `.shots.ron` shot list on one clock. `load` without `player` spawns one and returns its id; the
//! other actions take `player`, or act on the only player when there is exactly one. Calls land at
//! a frame boundary and the systems apply them later in that frame, so read a seek's effect (the
//! applied values, the camera pose) from the next call. Errors carry lowercase codes.
//!
//! `.seq.ron` v3 (ADR-2405) adds three `sequencer.*` methods on the same players:
//! `sequencer.spatial_voices` (where each spatial sound section emits from and the gain and pan
//! it plays at), `sequencer.sub_loops` (the pass and local time of every sub-sequence section in
//! effect, and the loop-depth check) and `sequencer.rename_binding` (one binding name renamed
//! everywhere the sequence refers to it, with the player's binding table entry).

use std::str::FromStr;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_framework::sequencer::{
    Binding, Channel, ChannelValue, ClockSource, EndPolicy, MAX_TAKE_BINDINGS, Sequence, SequenceBindings,
    SequenceClips, SequenceIssue, SequenceLibrary, SequencePlayer, SequenceSpawnables, SequenceStretches,
    SequenceSubtitleSettings, SequenceSubtitles, SequenceVideos, SequenceVoices, SoundSource, TakeBinding,
    TakeChannel, TakeConfig, TakeError, TakeRecorder, TakeTarget, VideoTarget, rename_binding, save_take,
};
use regia_framework::shots::{ShotSample, ShotSequence};
use regia_scene::components::{EntityGuid, Name};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most players one `query` lists.
const MAX_LISTED: usize = 64;
/// Most tracks a sequence loaded through the protocol may have.
const MAX_TRACKS: usize = 1024;
/// Most keys on one track, and most markers.
const MAX_KEYS: usize = 4096;
/// Most shots in a shot list loaded through the protocol.
const MAX_SHOTS: usize = 256;
/// Most bindings one call may set.
const MAX_BINDINGS: usize = 1024;

fn error(class: i64, code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(class, code, message, hint)
}

fn bad(code: &str, message: impl Into<String>) -> RpcError {
    error(
        INVALID_PARAMS,
        code,
        message,
        "see book/agents/sequence-player.md for the parameters",
    )
}

fn present<'a>(params: &'a Value, field: &str) -> Option<&'a Value> {
    params.get(field).filter(|v| !v.is_null())
}

fn entity_field(params: &Value, field: &str) -> Result<Option<Entity>, RpcError> {
    match present(params, field) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| bad("bad_entity", format!("`{field}` must be an entity id"))),
    }
}

fn number(params: &Value, field: &str) -> Result<Option<f64>, RpcError> {
    match present(params, field) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(Some)
            .ok_or_else(|| {
                bad(
                    "bad_setting",
                    format!("`{field}` must be a number of at least zero"),
                )
            }),
    }
}

fn flag(params: &Value, field: &str) -> Result<Option<bool>, RpcError> {
    match present(params, field) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| bad("bad_setting", format!("`{field}` must be a boolean"))),
    }
}

/// Player settings a call may change; parsed before anything is touched.
#[derive(Default)]
struct Settings {
    end: Option<EndPolicy>,
    clock: Option<ClockSource>,
    start_delay: Option<f64>,
    camera: Option<Option<Entity>>,
    blend_in: Option<f32>,
    blend_out: Option<f32>,
    takeover: Option<bool>,
    restore_on_stop: Option<bool>,
}

impl Settings {
    fn parse(params: &Value) -> Result<Self, RpcError> {
        let end = match present(params, "end") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(EndPolicy::from_name)
                    .ok_or_else(|| bad("bad_setting", "`end` must be \"hold\", \"loop\" or \"once\""))?,
            ),
        };
        let clock = match present(params, "clock") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(ClockSource::from_name)
                    .ok_or_else(|| bad("bad_setting", "`clock` must be \"gameplay\" or \"external\""))?,
            ),
        };
        // `camera: null` clears a named camera (back to the top camera); absent keeps it.
        let camera = match params.get("camera") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(_) => Some(entity_field(params, "camera")?),
        };
        Ok(Self {
            end,
            clock,
            start_delay: number(params, "start_delay")?,
            camera,
            blend_in: number(params, "blend_in")?.map(|s| s as f32),
            blend_out: number(params, "blend_out")?.map(|s| s as f32),
            takeover: flag(params, "takeover")?,
            restore_on_stop: flag(params, "restore_on_stop")?,
        })
    }

    fn apply(&self, player: &mut SequencePlayer) {
        if let Some(end) = self.end {
            player.end = end;
        }
        if let Some(clock) = self.clock {
            player.clock = clock;
        }
        if let Some(delay) = self.start_delay {
            player.start_delay = delay;
        }
        if let Some(camera) = self.camera {
            player.takeover.camera = camera;
        }
        if let Some(blend) = self.blend_in {
            player.takeover.blend_in = blend;
        }
        if let Some(blend) = self.blend_out {
            player.takeover.blend_out = blend;
        }
        if let Some(enabled) = self.takeover {
            player.takeover.enabled = enabled;
        }
        if let Some(restore) = self.restore_on_stop {
            player.restore_on_stop = restore;
        }
    }
}

/// The sequence of a `load` (`seq_ron` text or `sequence` JSON), checked for size.
fn sequence_from(params: &Value) -> Result<Option<Sequence>, RpcError> {
    let invalid = |e: regia_framework::sequencer::SequenceError| bad("bad_sequence", e.to_string());
    let sequence = match (present(params, "seq_ron"), present(params, "sequence")) {
        (Some(_), Some(_)) => return Err(bad("bad_sequence", "give `seq_ron` or `sequence`, not both")),
        (None, None) => return Ok(None),
        (Some(text), None) => {
            let text = text
                .as_str()
                .ok_or_else(|| bad("bad_sequence", "`seq_ron` must be the .seq.ron text"))?;
            Sequence::from_ron(text).map_err(invalid)?
        }
        (None, Some(value)) => {
            let mut sequence: Sequence = serde_json::from_value(value.clone())
                .map_err(|e| bad("bad_sequence", format!("`sequence`: {e}")))?;
            sequence
                .migrate_to_latest()
                .and_then(|_| sequence.validate())
                .map_err(invalid)?;
            sequence
        }
    };
    if sequence.tracks.len() > MAX_TRACKS || sequence.markers.len() > MAX_KEYS {
        return Err(bad(
            "too_large",
            format!("a sequence has at most {MAX_TRACKS} tracks and {MAX_KEYS} markers"),
        ));
    }
    for track in &sequence.tracks {
        let keys = track.keys.len();
        if keys > MAX_KEYS {
            return Err(bad(
                "too_large",
                format!("track `{}` has {keys} keys; the limit is {MAX_KEYS}", track.name),
            ));
        }
    }
    Ok(Some(sequence))
}

/// The shot list of a `load` (`shots_ron` text or `shots` JSON), checked for size.
fn shots_from(params: &Value) -> Result<Option<ShotSequence>, RpcError> {
    let invalid = |e: regia_framework::shots::ShotSequenceError| bad("bad_shots", e.to_string());
    let shots = match (present(params, "shots_ron"), present(params, "shots")) {
        (Some(_), Some(_)) => return Err(bad("bad_shots", "give `shots_ron` or `shots`, not both")),
        (None, None) => return Ok(None),
        (Some(text), None) => {
            let text = text
                .as_str()
                .ok_or_else(|| bad("bad_shots", "`shots_ron` must be the .shots.ron text"))?;
            ShotSequence::from_ron(text).map_err(invalid)?
        }
        (None, Some(value)) => {
            let mut shots: ShotSequence = serde_json::from_value(value.clone())
                .map_err(|e| bad("bad_shots", format!("`shots`: {e}")))?;
            shots
                .migrate_to_latest()
                .and_then(|_| shots.validate())
                .map_err(invalid)?;
            shots
        }
    };
    if shots.shots.len() > MAX_SHOTS {
        return Err(bad(
            "too_large",
            format!("a shot list has at most {MAX_SHOTS} shots"),
        ));
    }
    for shot in &shots.shots {
        let camera = &shot.camera;
        let most = camera
            .path
            .keys
            .len()
            .max(camera.fov.0.len())
            .max(camera.roll.0.len())
            .max(camera.rotation.len());
        if most > MAX_KEYS {
            return Err(bad(
                "too_large",
                format!(
                    "shot `{}` has {most} keys on one track; the limit is {MAX_KEYS}",
                    shot.name
                ),
            ));
        }
    }
    Ok(Some(shots))
}

/// What a `bindings` entry asks for, before it is checked against the world.
enum BindRequest {
    Unbind,
    Guid(Guid),
    Entity(Entity),
    Named(String),
}

fn bindings_from(params: &Value) -> Result<Vec<(String, BindRequest)>, RpcError> {
    let Some(value) = present(params, "bindings") else {
        return Ok(Vec::new());
    };
    let object = value.as_object().ok_or_else(|| {
        bad(
            "bad_binding",
            "`bindings` must be an object: { target: \"<guid>\" | <entity id> | { \"name\": \"Entity\" } | null }",
        )
    })?;
    if object.len() > MAX_BINDINGS {
        return Err(bad(
            "too_large",
            format!("at most {MAX_BINDINGS} bindings per call"),
        ));
    }
    let mut out = Vec::with_capacity(object.len());
    for (target, v) in object {
        let request = match v {
            Value::Null => BindRequest::Unbind,
            Value::String(text) => BindRequest::Guid(
                Guid::from_str(text).map_err(|e| bad("bad_binding", format!("`{target}`: {e}")))?,
            ),
            Value::Number(_) => BindRequest::Entity(
                v.as_u64()
                    .and_then(Entity::try_from_bits)
                    .ok_or_else(|| bad("bad_binding", format!("`{target}`: not an entity id")))?,
            ),
            Value::Object(map) => match crate::params::opt_str(map, "name")? {
                Some(name) if map.len() == 1 => BindRequest::Named(name.to_string()),
                _ => {
                    return Err(bad(
                        "bad_binding",
                        format!("`{target}`: use {{ \"name\": \"Entity\" }}"),
                    ));
                }
            },
            _ => {
                return Err(bad(
                    "bad_binding",
                    format!(
                        "`{target}`: a binding is a guid string, an entity id, {{ \"name\": .. }} or null"
                    ),
                ));
            }
        };
        out.push((target.clone(), request));
    }
    Ok(out)
}

/// The entity named `name`, preferring the ECS `Name` that `scene.spawn` inserts, then the
/// scene `Name`. The lowest entity bits win. A guid on that entity binds by guid.
fn find_named(world: &mut World, name: &str) -> Option<Binding> {
    let ecs = {
        let mut query = world.query::<(Entity, &regia_app::prelude::Name, Option<&EntityGuid>)>();
        query
            .iter(world)
            .filter(|(_, n, _)| n.as_str() == name)
            .min_by_key(|(e, _, _)| e.to_bits())
            .map(|(e, _, guid)| guid.map_or(Binding::Entity(e), |g| Binding::Guid(g.0)))
    };
    if ecs.is_some() {
        return ecs;
    }
    let mut query = world.query::<(Entity, &Name, Option<&EntityGuid>)>();
    query
        .iter(world)
        .filter(|(_, n, _)| n.as_str() == name)
        .min_by_key(|(e, _, _)| e.to_bits())
        .map(|(e, _, guid)| guid.map_or(Binding::Entity(e), |g| Binding::Guid(g.0)))
}

/// Resolves binding requests against the world (a name binds by guid when the entity has one).
fn resolve_bindings(
    world: &mut World,
    requests: Vec<(String, BindRequest)>,
) -> Result<Vec<(String, Option<Binding>)>, RpcError> {
    let mut out = Vec::with_capacity(requests.len());
    for (target, request) in requests {
        let binding = match request {
            BindRequest::Unbind => None,
            BindRequest::Guid(guid) => Some(Binding::Guid(guid)),
            BindRequest::Entity(entity) => {
                if world.get_entity(entity).is_err() {
                    return Err(error(
                        NOT_FOUND,
                        "unknown_entity",
                        format!("`{target}`: entity {} does not exist", entity.to_bits()),
                        "take entity ids from scene.query",
                    ));
                }
                Some(Binding::Entity(entity))
            }
            BindRequest::Named(name) => {
                let found = find_named(world, &name);
                Some(found.ok_or_else(|| {
                    error(
                        NOT_FOUND,
                        "unknown_entity",
                        format!("`{target}`: no entity is named `{name}`"),
                        "check the name with scene.query",
                    )
                })?)
            }
        };
        out.push((target, binding));
    }
    Ok(out)
}

/// `time` (seconds) or `frame` (at `fps`), never both.
fn time_param(params: &Value, fps: f32) -> Result<Option<f64>, RpcError> {
    let time = match present(params, "time") {
        None => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|t| t.is_finite() && *t >= 0.0)
                .ok_or_else(|| bad("bad_time", "`time` must be seconds, zero or more"))?,
        ),
    };
    let frame = match present(params, "frame") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| bad("bad_time", "`frame` must be a whole number of at least zero"))?,
        ),
    };
    match (time, frame) {
        (Some(_), Some(_)) => Err(bad("bad_time", "give `time` or `frame`, not both")),
        (Some(t), None) => Ok(Some(t)),
        (None, Some(f)) => Ok(Some(regia_framework::shots::time_of_frame(f, fps))),
        (None, None) => Ok(None),
    }
}

fn players(world: &mut World) -> Vec<Entity> {
    let mut query = world.query_filtered::<Entity, With<SequencePlayer>>();
    let mut list: Vec<Entity> = query.iter(world).collect();
    list.sort_unstable_by_key(|e| e.to_bits());
    list
}

/// The player a call acts on: the named one, or the only one.
fn target_player(world: &mut World, named: Option<Entity>) -> Result<Entity, RpcError> {
    if let Some(entity) = named {
        return if world.get::<SequencePlayer>(entity).is_some() {
            Ok(entity)
        } else {
            Err(error(
                NOT_FOUND,
                "unknown_player",
                format!("entity {} has no sequence player", entity.to_bits()),
                "list players with sequence.player {} (action query)",
            ))
        };
    }
    let all = players(world);
    match all.as_slice() {
        [] => Err(error(
            NOT_FOUND,
            "no_player",
            "no sequence player exists",
            "sequence.player { \"action\": \"load\", \"seq_ron\": \"...\" } creates one",
        )),
        [only] => Ok(*only),
        _ => Err(bad(
            "several_players",
            format!("{} players exist; pass `player`", all.len()),
        )),
    }
}

fn value_json(value: ChannelValue) -> Value {
    match value {
        ChannelValue::Float(v) => json!(v),
        ChannelValue::Vec3(v) => json!(v),
        ChannelValue::Bool(v) => json!(v),
    }
}

fn shot_json(shots: &ShotSequence, s: &ShotSample) -> Value {
    json!({
        "shot": s.shot,
        "shot_name": shots.shots.get(s.shot).map(|shot| shot.name.as_str()),
        "local_time": s.local_time,
        "eye": s.eye.to_array(),
        "rotation": s.rotation.to_array(),
        "fov_y_degrees": s.fov_y_degrees,
        "fade": s.fade,
        "fade_color": s.fade_color,
    })
}

/// What a player says about one instant, without moving its clock: keyed values (sub-sequence
/// ones with their depth and renamed target), clip blends, sounds and the shot.
fn sample_json(player: &SequencePlayer, time: f64) -> Value {
    let time = time.min(player.duration());
    let frame = match player.library() {
        Some(library) => player.sequence().sample_with(time, library),
        None => player.sequence().sample(time),
    };
    let values: Vec<Value> = frame
        .values
        .iter()
        .map(|v| {
            json!({
                "track": v.track,
                "name": v.name,
                "target": v.target,
                "property": v.property,
                "value": value_json(v.value),
                "depth": v.depth,
            })
        })
        .collect();
    let clips: Vec<Value> = frame
        .clips
        .iter()
        .map(|c| {
            json!({
                "track": c.name,
                "target": c.target,
                "depth": c.depth,
                "total_weight": c.total_weight(),
                "blends": c.blends.iter().map(|b| json!({
                    "section": b.section,
                    "clip": b.clip,
                    "clip_time": b.clip_time,
                    "looping": b.looping,
                    "weight": b.weight,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let sounds: Vec<Value> = frame
        .sounds
        .iter()
        .map(|s| {
            json!({
                "track": s.track,
                "path": s.path,
                "sound": s.sound,
                "position": s.position,
                "rate": s.rate,
                "volume_db": s.volume_db,
                "fade": s.fade,
                "remaining": s.remaining,
                "bus": s.bus,
                "spatial": s.spatial.is_some(),
                "emitter": s.emitter,
            })
        })
        .collect();
    let videos: Vec<Value> = frame
        .videos
        .iter()
        .map(|v| {
            json!({
                "track": v.track,
                "path": v.path,
                "video": v.video,
                "target": v.target.kind(),
                "binding": v.binding,
                "position": v.position,
                "rate": v.rate,
                "volume_db": v.volume_db,
                "looping": v.looping,
            })
        })
        .collect();
    let shot = player
        .shots()
        .and_then(|shots| shots.sample(time).map(|s| shot_json(shots, &s)));
    json!({ "time": time, "values": values, "clips": clips, "sounds": sounds, "videos": videos, "shot": shot })
}

fn binding_json(binding: Binding) -> Value {
    match binding {
        Binding::Guid(guid) => json!({ "guid": guid.to_string() }),
        Binding::Entity(entity) => json!({ "entity": entity.to_bits() }),
    }
}

/// A player's state: settings, clock, shots, takeover, bindings and reported issues.
fn state_json(world: &World, entity: Entity) -> Value {
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Value::Null;
    };
    let sequence = player.sequence();
    let bindings = world.get::<SequenceBindings>(entity);
    // Every track target, then any bound name no track uses.
    let mut targets: Vec<&str> = sequence.tracks.iter().map(|t| t.target.as_str()).collect();
    if let Some(bindings) = bindings {
        targets.extend(bindings.iter().map(|(name, _)| name));
    }
    targets.sort_unstable();
    targets.dedup();
    let resolved = player.resolved_targets();
    let binding_rows: Vec<Value> = targets
        .iter()
        .map(|&target| {
            let bound = sequence
                .tracks
                .iter()
                .zip(resolved)
                .find(|(t, _)| t.target == target)
                .and_then(|(_, e)| *e);
            json!({
                "target": target,
                "binding": bindings.and_then(|b| b.get(target)).map(binding_json),
                "entity": bound.map(Entity::to_bits),
                "bound": bound.is_some(),
                "tracks": sequence.tracks.iter().filter(|t| t.target == target).count(),
            })
        })
        .collect();
    let shots = player.shots().map(|shots| {
        json!({
            "count": shots.shots.len(),
            "duration": shots.duration(),
            "active": player.active_shot().map(|s| shot_json(shots, s)),
            "shot_changes": player.shot_changes(),
        })
    });
    let held = player
        .held_camera()
        .map(|(camera, weight)| json!({ "camera": camera.to_bits(), "weight": weight }));
    let issues: Vec<Value> = player
        .issues()
        .map(|issue| json!({ "kind": issue.kind(), "message": issue.to_string() }))
        .collect();
    let kinds = |kind: &str| sequence.tracks.iter().filter(|t| t.keys.kind() == kind).count();
    let voices = world
        .get_resource::<SequenceVoices>()
        .map_or(0, |v| v.voices(entity).len());
    json!({
        "version": sequence.version,
        "track_kinds": {
            "float": kinds("Float"), "vec3": kinds("Vec3"), "bool": kinds("Bool"),
            "clip": kinds("Clip"), "audio": kinds("Audio"), "sub": kinds("Sub"), "video": kinds("Video"),
        },
        "sub_sequences": sequence.sub_sequence_names(),
        "targets": player.target_names(),
        "flat_markers": player.flat_markers().len(),
        "voices": voices,
        "player": entity.to_bits(),
        "name": sequence.name,
        "status": player.status().name(),
        "time": player.time(),
        "frame": (player.time() * f64::from(sequence.fps) + 1.0e-6).floor() as u64,
        "fps": sequence.fps,
        "duration": player.duration(),
        "end": player.end.name(),
        "clock": player.clock.name(),
        "start_delay": player.start_delay,
        "delay_left": player.delay_left(),
        "cycle": player.cycle(),
        "skipped_cycles": player.skipped_cycles(),
        "start_stamp": player.start_stamp(),
        "tracks": sequence.tracks.len(),
        "markers": sequence.markers.len(),
        "markers_fired": player.markers_fired(),
        "shots": shots,
        "takeover": {
            "enabled": player.takeover.enabled,
            "camera": player.takeover.camera.map(Entity::to_bits),
            "blend_in": player.takeover.blend_in,
            "blend_out": player.takeover.blend_out,
            "held": held,
        },
        "bindings": binding_rows,
        "issues": issues,
        "prerolling": player.is_prerolling(),
        "restore_on_stop": player.restore_on_stop,
        "markers_skipped": player.markers_skipped(),
        "spawnables": spawnables_json(world, entity, player),
    })
}

/// The spawnable bindings of a player (`.seq.ron` v5, ADR-2948): each one's range, its prefab's
/// load state and, while it exists, the spawned entity and its guid.
fn spawnables_json(world: &World, entity: Entity, player: &SequencePlayer) -> Value {
    let runtime = world.get_resource::<SequenceSpawnables>();
    let rows: Vec<Value> = player
        .sequence()
        .spawnables
        .iter()
        .map(|s| {
            let live = runtime.and_then(|r| r.actor(entity, &s.binding));
            let load = runtime.and_then(|r| r.load_state(&s.prefab));
            json!({
                "binding": s.binding,
                "prefab": s.prefab,
                "start": s.start,
                "end": s.end,
                "keep_on_stop": s.keep_on_stop,
                "live": live.is_some(),
                "entity": live.map(|a| a.entity.to_bits()),
                "guid": live.map(|a| a.guid.to_string()),
                "load": load.map(|(state, _)| state),
                "load_code": load.and_then(|(_, code)| code),
            })
        })
        .collect();
    let stats = runtime.map(SequenceSpawnables::stats).unwrap_or_default();
    json!({
        "list": rows,
        "spawned": stats.spawned,
        "despawned": stats.despawned,
        "kept": stats.kept,
        "loads": stats.loads,
        "load_failures": stats.load_failures,
    })
}

/// `sequence.player { action?, player?, ... }`: see the module docs and the book page.
pub(crate) fn sequence_player(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = match present(params, "action") {
        None => "query",
        Some(v) => v
            .as_str()
            .ok_or_else(|| bad("bad_action", "`action` must be a string"))?,
    };
    const ACTIONS: &str =
        "query, load, play, pause, stop, seek, skip, sample, bind, set, clear, library, clip";
    if !matches!(
        action,
        "query"
            | "load"
            | "play"
            | "pause"
            | "stop"
            | "seek"
            | "skip"
            | "sample"
            | "bind"
            | "set"
            | "clear"
            | "library"
            | "clip"
    ) {
        return Err(bad(
            "bad_action",
            format!("unknown action `{action}` ({ACTIONS})"),
        ));
    }
    // Everything is parsed and checked before the world changes.
    let named = entity_field(params, "player")?;
    let settings = Settings::parse(params)?;
    let play_now = flag(params, "play")?;
    let requests = bindings_from(params)?;
    let (sequence, shots) = if action == "load" {
        (sequence_from(params)?, shots_from(params)?)
    } else {
        (None, None)
    };

    if action == "library" {
        return library_action(world, params);
    }
    if action == "clip" {
        return clip_action(world, params);
    }
    if action == "query" && named.is_none() {
        let list = players(world);
        let shown: Vec<Value> = list
            .iter()
            .take(MAX_LISTED)
            .map(|&e| state_json(world, e))
            .collect();
        return Ok(json!({ "action": action, "count": list.len(), "players": shown }));
    }

    let mut extra = Map::new();
    let entity = match action {
        "load" => {
            if sequence.is_none() && shots.is_none() {
                return Err(bad(
                    "bad_sequence",
                    "`load` needs a sequence (`seq_ron` or `sequence`), a shot list (`shots_ron` or `shots`), or both",
                ));
            }
            let bindings = resolve_bindings(world, requests)?;
            let existing = match named {
                Some(e) => Some(target_player(world, Some(e))?),
                None => None,
            };
            let entity = match existing {
                Some(entity) => {
                    let Some(mut player) = world.get_mut::<SequencePlayer>(entity) else {
                        return Err(RpcError::internal("the player vanished"));
                    };
                    if let Some(sequence) = sequence {
                        player.replace_sequence(sequence);
                    }
                    if shots.is_some() {
                        player.set_shots(shots);
                    }
                    entity
                }
                None => {
                    let sequence = sequence.unwrap_or_default();
                    let name = if sequence.name.is_empty() {
                        "sequence".to_string()
                    } else {
                        format!("sequence:{}", sequence.name)
                    };
                    let mut player = SequencePlayer::new(sequence);
                    if shots.is_some() {
                        player.set_shots(shots);
                    }
                    world
                        .spawn((player, SequenceBindings::default(), Name::new(name)))
                        .id()
                }
            };
            apply_bindings(world, entity, bindings);
            if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
                settings.apply(&mut player);
                if play_now == Some(true) {
                    player.play();
                }
            }
            entity
        }
        _ => {
            let entity = target_player(world, named)?;
            let bindings = resolve_bindings(world, requests)?;
            let fps = world
                .get::<SequencePlayer>(entity)
                .map_or(30.0, |p| p.sequence().fps);
            let time = time_param(params, fps)?;
            match action {
                "seek" if time.is_none() => {
                    return Err(bad("bad_time", "`seek` needs `time` (seconds) or `frame`"));
                }
                "sample" if time.is_none() => {
                    return Err(bad("bad_time", "`sample` needs `time` (seconds) or `frame`"));
                }
                _ => {}
            }
            apply_bindings(world, entity, bindings);
            if action == "clear" {
                world.despawn(entity);
                return Ok(json!({ "action": action, "player": entity.to_bits(), "cleared": true }));
            }
            let Some(mut player) = world.get_mut::<SequencePlayer>(entity) else {
                return Err(RpcError::internal("the player vanished"));
            };
            settings.apply(&mut player);
            match action {
                "play" => match time {
                    Some(time) => player.play_from(time),
                    None => player.play(),
                },
                "pause" => player.pause(),
                "stop" => player.stop(),
                "seek" => {
                    if let Some(time) = time {
                        player.seek(time);
                    }
                }
                // ADR-2948: the markers in between arrive with `skipped`; no time skips to the end.
                "skip" => match time {
                    Some(time) => player.skip_to(time),
                    None => player.skip_to_end(),
                },
                "sample" => {
                    if let Some(time) = time {
                        extra.insert("sample".into(), sample_json(&player, time));
                    }
                }
                // query, bind, set: the changes above are all there is.
                _ => {}
            }
            entity
        }
    };
    let mut out = json!({
        "action": action,
        "player": entity.to_bits(),
        "state": state_json(world, entity),
    });
    if let Some(object) = out.as_object_mut() {
        object.extend(extra);
    }
    Ok(out)
}

/// Most sequences the sub-sequence library may hold, and most clips the clip library may hold.
const MAX_LIBRARY: usize = 1024;

/// A library entry name: 1..=256 bytes.
fn library_name(params: &Value) -> Result<Option<String>, RpcError> {
    match present(params, "name") {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .filter(|n| !n.is_empty() && n.len() <= 256)
            .map(|n| Some(n.to_string()))
            .ok_or_else(|| bad("bad_name", "`name` must be a string of 1..=256 bytes")),
    }
}

/// `library { name?, seq_ron | sequence, remove? }`: adds (or with `remove: true` removes) a
/// sub-sequence of the [`SequenceLibrary`] under `name` (the name sub-sequence tracks use, usually
/// the project path), and lists the library. Players following it pick the change up next frame.
fn library_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = library_name(params)?;
    let remove = flag(params, "remove")?.unwrap_or(false);
    let sequence = sequence_from(params)?;
    match (&name, remove, &sequence) {
        (None, false, None) | (Some(_), true, None) | (Some(_), false, Some(_)) => {}
        (None, _, _) => {
            return Err(bad(
                "bad_name",
                "`library` with a sequence or `remove` needs `name`",
            ));
        }
        (Some(_), true, Some(_)) => {
            return Err(bad(
                "bad_sequence",
                "give a sequence to add, or `remove: true`, not both",
            ));
        }
        (Some(_), false, None) => {
            return Err(bad(
                "bad_sequence",
                "`library` with `name` needs a sequence (`seq_ron` or `sequence`) or `remove: true`",
            ));
        }
    }
    let mut library = world.get_resource_or_insert_with(SequenceLibrary::default);
    let mut removed = None;
    if let Some(name) = name {
        if remove {
            removed = Some(library.remove(&name));
        } else if let Some(sequence) = sequence {
            if library.get(&name).is_none() && library.len() >= MAX_LIBRARY {
                return Err(bad(
                    "too_large",
                    format!("the sequence library holds at most {MAX_LIBRARY} sequences"),
                ));
            }
            library.insert(name, sequence);
        }
    }
    let names: Vec<&str> = library.names().collect();
    Ok(json!({
        "action": "library",
        "sequences": names,
        "revision": library.revision(),
        "removed": removed,
    }))
}

/// `clip { name?, clip | asset, remove? }`: adds a clip to [`SequenceClips`] under `name` from its
/// JSON form (`clip`) or a cooked clip asset (`asset`, a project path or asset id), removes one,
/// or lists them.
fn clip_action(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = library_name(params)?;
    let remove = flag(params, "remove")?.unwrap_or(false);
    let clip = match (present(params, "clip"), present(params, "asset")) {
        (Some(_), Some(_)) => return Err(bad("bad_clip", "give `clip` or `asset`, not both")),
        (Some(value), None) => Some(
            serde_json::from_value::<regia_animation::AnimationClip>(value.clone())
                .map_err(|e| bad("bad_clip", format!("`clip`: {e}")))?,
        ),
        (None, Some(_)) => {
            let Some(server) = world.get_resource::<regia_asset::AssetServer>() else {
                return Err(error(
                    NOT_FOUND,
                    "bad_clip",
                    "no asset server to read the cooked clip from",
                    "pass the clip as JSON in `clip`",
                ));
            };
            let (path, bytes) = crate::assets::cooked_bytes(server, params, "asset")?;
            Some(
                regia_animation::cooked::load_cooked_clip(&bytes)
                    .map(|(_, clip)| clip)
                    .map_err(|e| bad("bad_clip", format!("{path} is not a cooked clip: {e}")))?,
            )
        }
        (None, None) => None,
    };
    if let Some(clip) = &clip
        && !(clip.duration.is_finite() && clip.duration >= 0.0)
    {
        return Err(bad(
            "bad_clip",
            format!("clip duration {} must be zero or more", clip.duration),
        ));
    }
    if (clip.is_some() || remove) && name.is_none() {
        return Err(bad("bad_name", "`clip` with a clip or `remove` needs `name`"));
    }
    if clip.is_some() && remove {
        return Err(bad("bad_clip", "give a clip to add, or `remove: true`, not both"));
    }
    let mut clips = world.get_resource_or_insert_with(SequenceClips::default);
    let mut removed = None;
    if let Some(name) = name {
        if remove {
            removed = Some(clips.remove(&name));
        } else if let Some(clip) = clip {
            if clips.get(&name).is_none() && clips.len() >= MAX_LIBRARY {
                return Err(bad(
                    "too_large",
                    format!("the clip library holds at most {MAX_LIBRARY} clips"),
                ));
            }
            clips.insert(name, clip);
        }
    }
    let names: Vec<&str> = clips.names().collect();
    Ok(json!({ "action": "clip", "clips": names, "removed": removed }))
}

fn apply_bindings(world: &mut World, entity: Entity, bindings: Vec<(String, Option<Binding>)>) {
    if bindings.is_empty() {
        return;
    }
    let Ok(mut player) = world.get_entity_mut(entity) else {
        return;
    };
    if player.get::<SequenceBindings>().is_none() {
        player.insert(SequenceBindings::default());
    }
    let Some(mut table) = player.get_mut::<SequenceBindings>() else {
        return;
    };
    for (target, binding) in bindings {
        match binding {
            Some(binding) => table.bind(target, binding),
            None => {
                table.unbind(&target);
            }
        }
    }
}

/// The player a `sequencer.*` call names (`player`), or the only one.
fn named_player(world: &mut World, params: &Value) -> Result<Entity, RpcError> {
    let named = entity_field(params, "player")?;
    target_player(world, named)
}

/// `sequencer.spatial_voices { player?, all? }`: every spatial sound section in effect at the
/// player's time (where it emits from, its binding, placement, Doppler factor and occlusion
/// switch, v5) and the voice that plays it (gain, pan, volume, pitch, Doppler ratio and velocity,
/// occlusion amount and cutoff, whether it plays a stretch) once the audio sync has run. `all`
/// lists the flat sections too (their time scale mode and stretch, v5). `stretch_cache` reports
/// the pitch-preserving stretches rendered ahead (ADR-3197).
pub(crate) fn spatial_voices(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = named_player(world, params)?;
    let all = flag(params, "all")?.unwrap_or(false);
    let (voices, keys) = world
        .get_resource::<SequenceVoices>()
        .map_or((Vec::new(), Vec::new()), |v| {
            let voices = v.voices(entity);
            let keys = voices
                .iter()
                .map(|(path, _)| v.occlusion_key(entity, path).map(Entity::to_bits))
                .collect();
            (voices, keys)
        });
    let stretch_cache = world
        .get_resource::<SequenceStretches>()
        .map(|s| serde_json::to_value(s.stats()).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    let frame = player.frame();
    let list: Vec<Value> = frame
        .sounds
        .iter()
        .filter(|sound| all || sound.spatial.is_some())
        .map(|sound| {
            let found = voices.iter().position(|(path, _)| *path == sound.path);
            let voice = found.map(|i| &voices[i].1);
            let bound = sound
                .emitter
                .and_then(|name| player.target_entities().get(name))
                .map(|e| e.to_bits());
            let mut out = json!({
                "track": sound.track,
                "path": sound.path,
                "sound": sound.sound,
                "rate": sound.rate,
                "time_scale_mode": sound.time_scale_mode.name(),
                "spatial": sound.spatial.is_some(),
                "voice": voice.map(|v| json!({
                    "id": v.id.0,
                    "volume_db": v.volume_db,
                    "pan": v.pan,
                    "gain": v.spatial_gain,
                    "position": v.position,
                    "pitch": v.pitch,
                    "doppler": v.doppler,
                    "velocity": v.velocity,
                    "occlusion": v.occlusion,
                    "cutoff_hz": v.cutoff_hz,
                    "stretched": v.stretched,
                })),
            });
            if let Some(spatial) = sound.spatial {
                out["source"] = match &spatial.source {
                    SoundSource::Binding(name) => json!({ "binding": name }),
                    SoundSource::Position(p) => json!({ "position": p }),
                };
                out["emitter"] = json!(sound.emitter);
                out["bound_entity"] = json!(bound);
                out["min_distance"] = json!(spatial.min_distance);
                out["max_distance"] = json!(spatial.max_distance);
                out["rolloff"] = serde_json::to_value(spatial.rolloff).unwrap_or(Value::Null);
                out["spatial_blend"] = json!(spatial.spatial_blend);
                out["doppler_factor"] = json!(spatial.doppler);
                out["occlusion"] = json!(spatial.occlusion);
                out["occlusion_key"] = json!(found.and_then(|i| keys[i]));
            }
            out
        })
        .collect();
    Ok(json!({
        "player": entity.to_bits(),
        "time": player.time(),
        "voices": list,
        "stretch_cache": stretch_cache,
    }))
}

/// `sequencer.subtitles { player? }`: the subtitle cues the player's sound sections show now
/// (ADR-3334): the section, its dialogue line, speaker, cue and text, and the caption set on the
/// board. Also the settings (`enabled`, `burn_in`, the opt-outs).
pub(crate) fn subtitles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = named_player(world, params)?;
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    let status = player.status().name();
    let time = player.time();
    let cues: Vec<Value> = world
        .get_resource::<SequenceSubtitles>()
        .map(|s| s.active(entity))
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            json!({
                "path": a.path,
                "sound": a.sound,
                "dialogue": a.dialogue,
                "line": a.line,
                "speaker": a.speaker,
                "cue": a.cue,
                "text": a.text,
                "locale": a.locale,
                "sound_start": a.sound_span[0],
                "sound_end": a.sound_span[1],
                "caption": a.caption.map(|c| c.0),
            })
        })
        .collect();
    Ok(json!({
        "player": entity.to_bits(),
        "status": status,
        "time": time,
        "cues": cues,
        "settings": subtitle_settings_json(world),
    }))
}

fn subtitle_settings_json(world: &World) -> Value {
    let settings = world
        .get_resource::<SequenceSubtitleSettings>()
        .cloned()
        .unwrap_or_default();
    json!({
        "enabled": settings.enabled,
        "burn_in": settings.burn_in,
        "opt_out_tracks": settings.opt_out_tracks,
        "opt_out_sounds": settings.opt_out_sounds,
    })
}

fn name_set(params: &Value, field: &str) -> Result<Option<std::collections::BTreeSet<String>>, RpcError> {
    let Some(v) = present(params, field) else {
        return Ok(None);
    };
    let list = v
        .as_array()
        .ok_or_else(|| bad("bad_setting", format!("`{field}` must be a list of names")))?;
    if list.len() > MAX_TRACKS {
        return Err(bad("bad_setting", format!("`{field}` lists too many names")));
    }
    list.iter()
        .map(|n| {
            n.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad("bad_setting", format!("`{field}` must be a list of names")))
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

/// `sequencer.subtitle_settings { enabled?, burn_in?, opt_out_tracks?, opt_out_sounds? }`: sets
/// the given settings of sequence subtitles and returns all of them (no parameters: just reads).
pub(crate) fn subtitle_settings(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = flag(params, "enabled")?;
    let burn_in = flag(params, "burn_in")?;
    let tracks = name_set(params, "opt_out_tracks")?;
    let sounds = name_set(params, "opt_out_sounds")?;
    let mut settings = world.get_resource_or_init::<SequenceSubtitleSettings>();
    if let Some(v) = enabled {
        settings.enabled = v;
    }
    if let Some(v) = burn_in {
        settings.burn_in = v;
    }
    if let Some(v) = tracks {
        settings.opt_out_tracks = v;
    }
    if let Some(v) = sounds {
        settings.opt_out_sounds = v;
    }
    Ok(subtitle_settings_json(world))
}

/// One video target as JSON: its kind and the fields it has.
fn video_target_json(target: &VideoTarget) -> Value {
    let mut out = json!({ "kind": target.kind(), "binding": target.binding() });
    match target {
        VideoTarget::Fullscreen { layer, fit } => {
            out["layer"] = json!(layer);
            out["fit"] = serde_json::to_value(fit).unwrap_or(Value::Null);
        }
        VideoTarget::Screen { slot, .. } => {
            out["slot"] = serde_json::to_value(slot).unwrap_or(Value::Null);
        }
        VideoTarget::Player(_) | VideoTarget::UiImage(_) => {}
    }
    out
}

/// `sequencer.video_sections { player? }`: the player's own video sections, the sessions in effect
/// (sub-sequences included) with the frame each shows and its drift, and the sequence's problems.
pub(crate) fn video_sections(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = named_player(world, params)?;
    let manual = matches!(
        world.get_resource::<TimeUpdateStrategy>(),
        Some(TimeUpdateStrategy::ManualDuration(_))
    );
    let videos = world.get_resource::<SequenceVideos>();
    let frame_exact = manual || videos.is_some_and(|v| v.force_exact);
    let (opened, closed) = videos.map_or((0, 0), |v| (v.opened(), v.closed()));
    let active: Vec<Value> = videos
        .map(|v| v.sections(entity))
        .unwrap_or_default()
        .iter()
        .map(|s| {
            json!({
                "track": s.track,
                "path": s.path,
                "video": s.video,
                "target": s.target,
                "media": s.media.map(Entity::to_bits),
                "state": s.state,
                "source_time": s.source_time,
                "frame_shown": s.frame_shown,
                "picture_time": s.picture_time,
                "drift": s.drift,
                "audio_drift": s.audio_drift,
                "certain": s.certain,
                "exact": s.exact,
                "seeks": s.seeks,
                "resyncs": s.resyncs,
                "frames_dropped": s.frames_dropped,
                "error": s.error,
            })
        })
        .collect();
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    let time = player.time();
    let mut sections = Vec::new();
    for track in &player.sequence().tracks {
        let Channel::Video(list) = &track.keys else {
            continue;
        };
        for (index, s) in list.iter().enumerate() {
            sections.push(json!({
                "track": track.name,
                "index": index,
                "video": s.video,
                "start": s.start,
                "end": s.end,
                "offset": s.offset,
                "target": video_target_json(&s.target),
                "volume_db": s.volume_db,
                "looping": s.looping,
                "in_effect": track.enabled
                    && player.is_active()
                    && time >= f64::from(s.start)
                    && time < f64::from(s.end),
            }));
        }
    }
    let issues: Vec<Value> = player
        .issues()
        .filter(|issue| {
            matches!(
                issue,
                SequenceIssue::VideoMissing { .. }
                    | SequenceIssue::VideoFailed { .. }
                    | SequenceIssue::VideoUnavailable { .. }
            )
        })
        .map(|issue| json!({ "kind": issue.kind(), "message": issue.to_string() }))
        .collect();
    Ok(json!({
        "player": entity.to_bits(),
        "time": time,
        "frame_exact": frame_exact,
        "opened": opened,
        "closed": closed,
        "sections": sections,
        "active": active,
        "issues": issues,
    }))
}

/// `sequencer.sub_loops { player?, time? }`: every sub-sequence section in effect at `time` (the
/// player's time by default) with its repeat, pass length, pass and local time, after checking
/// the tree: a tree whose looping sections nest too deep answers `seq_loop_too_deep`.
pub(crate) fn sub_loops(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = named_player(world, params)?;
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    let time = match number(params, "time")? {
        Some(t) => t,
        None => player.time(),
    };
    let empty = SequenceLibrary::default();
    let library = player.library().unwrap_or(&empty);
    if let Err(e) = library.check(player.sequence()) {
        let code = e.code();
        let hint = if code == "seq_loop_too_deep" {
            "looping sub-sequence sections nest at most 4 deep; set an inner section's repeat to once"
        } else {
            "the sub-sequence tree is incomplete or refused; sequence.player { \"action\": \"query\" } lists the issue"
        };
        return Err(error(INVALID_PARAMS, code, e.to_string(), hint));
    }
    let passes: Vec<Value> = player
        .sequence()
        .sub_passes(time, library)
        .iter()
        .map(|p| {
            json!({
                "path": p.path,
                "track": p.track,
                "sequence": p.sequence,
                "repeat": p.repeat.mode_name(),
                "count": match p.repeat {
                    regia_framework::sequencer::SubRepeat::Count(n) => Some(n),
                    _ => None,
                },
                "pass_length": p.pass_length,
                "pass": p.pass,
                "local_time": p.local_time,
                "depth": p.depth,
            })
        })
        .collect();
    Ok(json!({ "player": entity.to_bits(), "time": time, "sections": passes }))
}

/// `sequencer.rename_binding { player?, from, to }`: renames binding `from` to `to` in the
/// player's sequence (every track, spatial sound section and sub-sequence binding table that
/// refers to it, through the player's sub-sequence library) and moves the player's binding table
/// entry, so the same entity stays bound. No undo here: the editor's section strip runs the same
/// rename as one undo step.
pub(crate) fn rename_binding_method(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = named_player(world, params)?;
    let name = |field: &str| -> Result<String, RpcError> {
        params
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| bad("bad_name", format!("`{field}` (a string) is required")))
            .map(str::to_string)
    };
    let (from, to) = (name("from")?, name("to")?);
    let Some(player) = world.get::<SequencePlayer>(entity) else {
        return Err(bad("unknown_player", "the player is gone"));
    };
    let mut sequence = player.sequence().clone();
    let done = rename_binding(&mut sequence, player.library(), &from, &to).map_err(|e| {
        let hint = match e.code {
            "seq_binding_unknown" => "sequence.player { \"action\": \"query\" } lists the targets",
            "seq_binding_in_use" => "pick a name the sequence does not bind yet",
            _ => "see book/agents/sequence-player.md (Sequencer v3)",
        };
        let class = if e.code == "seq_binding_unknown" {
            NOT_FOUND
        } else {
            INVALID_PARAMS
        };
        error(class, e.code, e.message, hint)
    })?;
    let mut table = world.get::<SequenceBindings>(entity).cloned();
    if let Some(table) = table.as_mut() {
        if table.get(&to).is_some() {
            return Err(error(
                INVALID_PARAMS,
                "seq_binding_table_in_use",
                format!("the player already binds `{to}`"),
                "unbind it first: sequence.player { \"action\": \"bind\", \"bindings\": { \"<to>\": null } }",
            ));
        }
        if let Some(binding) = table.get(&from) {
            table.unbind(&from);
            table.bind(to.clone(), binding);
        }
    }
    if let Some(mut player) = world.get_mut::<SequencePlayer>(entity) {
        player.replace_sequence(sequence);
    }
    if let Some(table) = table {
        world.entity_mut(entity).insert(table);
    }
    Ok(json!({
        "player": entity.to_bits(),
        "from": from,
        "to": to,
        "tracks": done.tracks,
        "sounds": done.sounds,
        "videos": done.videos,
        "sub_sections": done.sub_sections,
        "unresolved": done.unresolved,
    }))
}

// ---------------------------------------------------------------------------------------------
// Take recorder (ADR-2637, book/agents/sequence-take-recorder.md): `sequence.record_start`,
// `sequence.record_stop`, `sequence.record_state`.
// ---------------------------------------------------------------------------------------------

fn take_error(e: TakeError) -> RpcError {
    let (class, hint) = match &e {
        TakeError::Busy => (
            INVALID_PARAMS,
            "stop it with sequence.record_stop (or pass cancel: true) first",
        ),
        TakeError::Unsaved => (
            INVALID_PARAMS,
            "save the kept take with sequence.record_stop { sequence }, or drop it with { cancel: true }",
        ),
        TakeError::NotRecording => (
            INVALID_PARAMS,
            "start one with sequence.record_start; sequence.record_state shows the status",
        ),
        TakeError::Empty(_) => (
            INVALID_PARAMS,
            "let the game run at least one frame after the countdown (play.step), or pass include_driven",
        ),
        TakeError::Io(_) => (
            crate::protocol::INTERNAL_ERROR,
            "check disk space and permissions; the take is kept: call sequence.record_stop again with a sequence path",
        ),
        TakeError::SourceInvalid(_) => (
            INVALID_PARAMS,
            "fix the source sequence or record into another path; the take is kept",
        ),
        TakeError::NumbersExhausted(_) => (
            INVALID_PARAMS,
            "move old takes away or record into another sequence; the take is kept",
        ),
        _ => (
            INVALID_PARAMS,
            "see book/agents/sequence-take-recorder.md for the parameters",
        ),
    };
    error(class, e.code(), e.to_string(), hint)
}

fn positive(params: &Value, field: &str) -> Result<Option<f32>, RpcError> {
    match present(params, field) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && *f > 0.0 && *f <= 1000.0)
            .map(|f| Some(f as f32))
            .ok_or_else(|| {
                bad(
                    "take_bad_tolerance",
                    format!("`{field}` must be a number in (0, 1000]"),
                )
            }),
    }
}

fn take_binding_from(world: &mut World, value: &Value) -> Result<TakeBinding, RpcError> {
    let bad_binding = |m: String| bad("take_bad_binding", m);
    let map = value.as_object().ok_or_else(|| {
        bad_binding(
            "a binding is { name, guid | entity | entity_name | camera: true, channels: [..] }".into(),
        )
    })?;
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_binding("a binding needs `name` (the track target)".into()))?
        .to_string();
    let mut targets = Vec::new();
    if let Some(guid) = map.get("guid").filter(|v| !v.is_null()) {
        let text = guid
            .as_str()
            .ok_or_else(|| bad_binding(format!("`{name}`: `guid` must be a string")))?;
        let guid = Guid::from_str(text).map_err(|e| bad_binding(format!("`{name}`: {e}")))?;
        targets.push(TakeTarget::Binding(Binding::Guid(guid)));
    }
    if let Some(entity) = map.get("entity").filter(|v| !v.is_null()) {
        let entity = entity
            .as_u64()
            .and_then(Entity::try_from_bits)
            .ok_or_else(|| bad("bad_entity", format!("`{name}`: `entity` must be an entity id")))?;
        if world.get_entity(entity).is_err() {
            return Err(error(
                NOT_FOUND,
                "unknown_entity",
                format!("`{name}`: entity {} does not exist", entity.to_bits()),
                "take entity ids from scene.query",
            ));
        }
        targets.push(TakeTarget::Binding(Binding::Entity(entity)));
    }
    if let Some(entity_name) = map.get("entity_name").filter(|v| !v.is_null()) {
        let entity_name = entity_name
            .as_str()
            .ok_or_else(|| bad_binding(format!("`{name}`: `entity_name` must be a string")))?;
        let resolved = resolve_bindings(
            world,
            vec![(name.clone(), BindRequest::Named(entity_name.to_string()))],
        )?;
        if let Some((_, Some(binding))) = resolved.into_iter().next() {
            targets.push(TakeTarget::Binding(binding));
        }
    }
    if crate::params::opt_bool(map, "camera")? == Some(true) {
        targets.push(TakeTarget::ActiveCamera);
    }
    let [target] = targets[..] else {
        return Err(bad_binding(format!(
            "`{name}`: give exactly one of guid, entity, entity_name or camera: true"
        )));
    };
    let channels = match map.get("channels") {
        None | Some(Value::Null) => vec![if target == TakeTarget::ActiveCamera {
            TakeChannel::Camera
        } else {
            TakeChannel::Transform
        }],
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .ok_or_else(|| bad("take_bad_channel", format!("`{name}`: channels are strings")))
                    .and_then(|s| TakeChannel::parse(s).map_err(take_error))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(bad(
                "take_bad_channel",
                format!("`{name}`: `channels` must be an array of channel names"),
            ));
        }
    };
    Ok(TakeBinding {
        name,
        target,
        channels,
    })
}

fn take_config_from(world: &mut World, params: &Value) -> Result<TakeConfig, RpcError> {
    let mut config = TakeConfig::default();
    match present(params, "bindings") {
        None => {}
        Some(Value::Array(items)) => {
            if items.len() > MAX_TAKE_BINDINGS {
                return Err(bad(
                    "take_too_many",
                    format!("at most {MAX_TAKE_BINDINGS} bindings"),
                ));
            }
            for item in items {
                let binding = take_binding_from(world, item)?;
                config.bindings.push(binding);
            }
        }
        Some(_) => return Err(bad("take_bad_binding", "`bindings` must be an array")),
    }
    config.audio_cues = flag(params, "audio_cues")?.unwrap_or(false);
    config.include_driven = flag(params, "include_driven")?.unwrap_or(false);
    if let Some(v) = present(params, "countdown") {
        config.countdown = v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| bad("bad_setting", "`countdown` must be a whole number of frames"))?;
    }
    if let Some(fps) = positive(params, "fps")? {
        config.fps = fps;
    }
    config.cut_distance = positive(params, "cut_distance")?;
    if let Some(tolerance) = present(params, "tolerance") {
        if !tolerance.is_object() {
            return Err(bad("take_bad_tolerance", "`tolerance` must be an object"));
        }
        let t = &mut config.tolerance;
        for (field, slot) in [
            ("position", &mut t.position),
            ("rotation", &mut t.rotation),
            ("scale", &mut t.scale),
            ("property", &mut t.property),
            ("clip_time", &mut t.clip_time),
        ] {
            if let Some(v) = positive(tolerance, field)? {
                *slot = v;
            }
        }
    }
    config.validate().map_err(take_error)?;
    Ok(config)
}

fn take_state_json(recorder: &TakeRecorder) -> Value {
    let bindings: Vec<Value> = recorder
        .binding_states()
        .into_iter()
        .map(|b| {
            json!({
                "name": b.name,
                "entity": b.entity.map(Entity::to_bits),
                "channels": b.channels,
                "frames": b.frames,
                "missing": b.missing,
                "excluded": b.excluded,
                "cuts": b.cuts,
            })
        })
        .collect();
    json!({
        "status": recorder.status().name(),
        "countdown_left": recorder.countdown_left(),
        "frames": recorder.frames(),
        "duration": recorder.duration(),
        "cues": recorder.cues(),
        "dropped_cues": recorder.dropped_cues(),
        "bindings": bindings,
        "issues": recorder.issues().collect::<Vec<_>>(),
        "unsaved_take": recorder.has_unsaved(),
        "takes": recorder.takes(),
    })
}

fn recorder_mut(world: &mut World) -> Mut<'_, TakeRecorder> {
    if !world.contains_resource::<TakeRecorder>() {
        world.init_resource::<TakeRecorder>();
    }
    world.resource_mut::<TakeRecorder>()
}

/// `sequence.record_start`: starts recording a take.
pub(crate) fn record_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let config = take_config_from(world, params)?;
    let mut recorder = recorder_mut(world);
    recorder.start(config).map_err(take_error)?;
    Ok(take_state_json(&recorder))
}

/// `sequence.record_state`: the recorder's status, clock and bindings. A `read` method (ADR-5957):
/// it borrows the world immutably, so an app without a recorder yet answers an idle one's state
/// and still has no recorder afterwards.
pub(crate) fn record_state(world: &World, _params: &Value) -> Result<Value, RpcError> {
    Ok(match world.get_resource::<TakeRecorder>() {
        Some(recorder) => take_state_json(recorder),
        None => take_state_json(&TakeRecorder::default()),
    })
}

/// The project root files are written under (the asset server's, the agent's project dir, or the
/// working directory).
fn project_root(world: &World) -> Option<std::path::PathBuf> {
    world
        .get_resource::<regia_asset::AssetServer>()
        .map(|s| s.project_root().to_path_buf())
        .or_else(|| {
            world
                .get_resource::<crate::AgentSettings>()
                .and_then(|s| s.project_dir.clone())
        })
        .or_else(|| std::env::current_dir().ok())
}

/// Refuses a source whose existing parent directory leads out of the project through a symlink.
fn check_inside(root: &std::path::Path, source: &str) -> Result<(), RpcError> {
    let escape = || {
        bad(
            "take_bad_path",
            format!("`{source}` leads out of the project through a symlink"),
        )
    };
    let Ok(root) = root.canonicalize() else {
        return Ok(());
    };
    let mut dir = root.join(source);
    while !dir.exists() {
        if !dir.pop() {
            return Ok(());
        }
    }
    let real = dir.canonicalize().map_err(|_| escape())?;
    if real.starts_with(&root) {
        Ok(())
    } else {
        Err(escape())
    }
}

/// `sequence.record_stop`: stops the take (or saves a kept one) and writes it next to `sequence`.
pub(crate) fn record_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if flag(params, "cancel")? == Some(true) {
        recorder_mut(world).cancel().map_err(take_error)?;
        return Ok(json!({ "cancelled": true }));
    }
    let source = match present(params, "sequence") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| {
                    bad(
                        "take_bad_path",
                        "`sequence` must be a project-relative .seq.ron path",
                    )
                })?
                .to_string(),
        ),
    };
    if let Some(source) = &source {
        regia_framework::sequencer::recorder::split_source(source).map_err(take_error)?;
    }
    let at = number(params, "at")?.unwrap_or(0.0) as f32;
    let take = {
        let mut recorder = recorder_mut(world);
        if recorder.status().is_active() {
            recorder.stop().map_err(take_error)?
        } else {
            recorder
                .take_unsaved()
                .ok_or_else(|| take_error(TakeError::NotRecording))?
        }
    };
    let tracks: Vec<Value> = take
        .tracks
        .iter()
        .map(|(name, samples, keys)| json!({ "name": name, "samples": samples, "keys": keys }))
        .collect();
    let excluded: Vec<Value> = take
        .excluded
        .iter()
        .map(|(name, reason)| json!({ "name": name, "reason": reason }))
        .collect();
    let mut out = json!({
        "frames": take.frames,
        "duration": take.duration,
        "tracks": tracks,
        "markers": take.sequence.markers.iter().map(|m| json!({ "name": m.name, "time": m.time })).collect::<Vec<_>>(),
        "excluded": excluded,
        "dropped_cues": take.dropped_cues,
        "issues": take.issues,
    });
    let Some(source) = source else {
        let text = match take.sequence.to_ron() {
            Ok(text) => text,
            Err(e) => {
                recorder_mut(world).keep_unsaved(take);
                return Err(take_error(TakeError::Invalid(e.to_string())));
            }
        };
        out["seq_ron"] = Value::String(text);
        return Ok(out);
    };
    let Some(root) = project_root(world) else {
        recorder_mut(world).keep_unsaved(take);
        return Err(error(
            crate::protocol::INTERNAL_ERROR,
            "take_io",
            "no project root to write the take under",
            "run the agent inside a project",
        ));
    };
    if let Err(e) = check_inside(&root, &source) {
        recorder_mut(world).keep_unsaved(take);
        return Err(e);
    }
    let files = match save_take(&root, &source, &take.sequence, at) {
        Ok(files) => files,
        Err(e) => {
            recorder_mut(world).keep_unsaved(take);
            return Err(take_error(e));
        }
    };
    // The take plays at once inside its source: a player that follows the library sees it.
    if let Some(mut library) = world.get_resource_mut::<SequenceLibrary>() {
        let mut named = take.sequence.clone();
        named.name = files
            .take
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .trim_end_matches(".seq.ron")
            .to_string();
        library.insert(files.take.clone(), named);
    }
    out["take"] = json!(files.take);
    out["number"] = json!(files.number);
    out["sequence"] = json!(files.source);
    out["section_track"] = json!(files.section_track);
    out["created_sequence"] = json!(files.created_source);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_framework::sequencer::SequencerPlugin;
    use regia_render::Camera;
    use regia_scene::Transform;

    use super::*;

    const SEQ: &str = r#"(version: 1, name: "intro",
        tracks: [(name: "move", target: "hero", property: "transform.position",
            keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 2.0, value: (4.0, 0.0, 0.0))]))],
        markers: [(name: "go", time: 0.0), (name: "half", time: 1.0)])"#;

    const SHOTS: &str = r#"(version: 2, shots: [(name: "wide", duration: 2.0, camera: (path: (interp: Linear,
        keys: [(time: 0.0, position: (0.0, 5.0, 20.0))], look_at: Point((0.0, 0.0, 0.0)))))])"#;

    fn code(e: RpcError) -> String {
        e.data
            .and_then(|d| d["code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// `sequence.record_state` is a read (ADR-5960 (6)): on a world without a recorder it reports
    /// an idle one and inserts none, and with a recorder it leaves it unchanged.
    #[test]
    fn record_state_reads_without_inserting_or_touching_the_recorder() {
        let mut world = World::new();
        let state = record_state(&world, &json!({})).expect("state");
        assert_eq!(state["status"], "idle", "{state}");
        assert!(
            !world.contains_resource::<TakeRecorder>(),
            "the read inserted a TakeRecorder"
        );
        world.init_resource::<TakeRecorder>();
        world.clear_trackers();
        let before = world.change_tick();
        record_state(&world, &json!({})).expect("state");
        assert!(
            !world.is_resource_changed::<TakeRecorder>(),
            "the read marked the recorder changed (tick {before:?})"
        );
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(250)));
        app
    }

    fn call(app: &mut App, params: Value) -> Result<Value, RpcError> {
        sequence_player(app.world_mut(), &params)
    }

    #[test]
    fn load_bind_play_and_query_drive_the_target() {
        let mut app = app();
        let hero = app
            .world_mut()
            .spawn((
                Transform::default(),
                Name::new("Hero"),
                EntityGuid(Guid::from_u128(9)),
            ))
            .id();
        let out = call(
            &mut app,
            json!({
                "action": "load", "seq_ron": SEQ, "end": "once",
                "bindings": { "hero": { "name": "Hero" } }, "play": true
            }),
        )
        .expect("load");
        let player = out["player"].as_u64().expect("player id");
        assert_eq!(out["state"]["status"], "playing");
        assert_eq!(out["state"]["end"], "once");
        assert_eq!(
            out["state"]["bindings"][0]["binding"]["guid"],
            json!(Guid::from_u128(9).to_string()),
            "a name with a guid binds by guid"
        );
        for _ in 0..4 {
            app.update();
        }
        let x = app.world().get::<Transform>(hero).expect("hero").translation.x;
        assert!((x - 2.0).abs() < 1.0e-4, "one second in: {x}");
        let out = call(&mut app, json!({ "player": player })).expect("query");
        assert_eq!(out["state"]["bindings"][0]["bound"], true);
        assert_eq!(out["state"]["markers_fired"], 2);
        let list = call(&mut app, json!({})).expect("list");
        assert_eq!(list["count"], 1);
    }

    #[test]
    fn seek_sample_pause_and_set_act_on_the_only_player() {
        let mut app = app();
        call(
            &mut app,
            json!({ "action": "load", "seq_ron": SEQ, "shots_ron": SHOTS }),
        )
        .expect("load");
        let out = call(&mut app, json!({ "action": "sample", "time": 1.0 })).expect("sample");
        assert_eq!(out["sample"]["values"][0]["value"], json!([2.0, 0.0, 0.0]));
        assert_eq!(out["sample"]["shot"]["shot_name"], "wide");
        assert_eq!(out["state"]["time"], 0.0, "sampling does not move the clock");
        let out = call(&mut app, json!({ "action": "seek", "frame": 45 })).expect("seek");
        assert_eq!(out["state"]["time"], 1.5);
        let out = call(&mut app, json!({ "action": "play" })).expect("play");
        assert_eq!(out["state"]["status"], "playing");
        let out = call(&mut app, json!({ "action": "pause" })).expect("pause");
        assert_eq!(out["state"]["status"], "paused");
        let out = call(
            &mut app,
            json!({ "action": "set", "end": "loop", "blend_in": 0.0, "clock": "external", "takeover": false }),
        )
        .expect("set");
        assert_eq!(out["state"]["end"], "loop");
        assert_eq!(out["state"]["clock"], "external");
        assert_eq!(out["state"]["takeover"]["enabled"], false);
        assert_eq!(out["state"]["takeover"]["blend_in"], 0.0);
        let out = call(&mut app, json!({ "action": "stop" })).expect("stop");
        assert_eq!(out["state"]["status"], "stopped");
        let out = call(&mut app, json!({ "action": "clear" })).expect("clear");
        assert_eq!(out["cleared"], true);
        assert_eq!(call(&mut app, json!({})).expect("list")["count"], 0);
    }

    #[test]
    fn a_shot_list_takes_over_the_camera_and_reports_it() {
        let mut app = app();
        let camera = app
            .world_mut()
            .spawn((Transform::default(), Camera::default()))
            .id();
        let out = call(
            &mut app,
            json!({ "action": "load", "shots_ron": SHOTS, "blend_in": 0.0, "play": true }),
        )
        .expect("load");
        let player = out["player"].clone();
        app.update();
        app.update();
        let eye = app.world().get::<Transform>(camera).expect("camera").translation;
        assert_eq!(eye.to_array(), [0.0, 5.0, 20.0]);
        let out = call(&mut app, json!({ "player": player })).expect("query");
        assert_eq!(out["state"]["takeover"]["held"]["camera"], camera.to_bits());
        assert_eq!(out["state"]["takeover"]["held"]["weight"], 1.0);
        assert_eq!(out["state"]["shots"]["active"]["shot_name"], "wide");
    }

    #[test]
    fn unbound_targets_and_unknown_properties_show_as_issues() {
        let mut app = app();
        let seq = r#"(version: 1, tracks: [
            (name: "a", target: "ghost", property: "transform.position", keys: Vec3([(time: 0.0, value: (1.0, 0.0, 0.0))])),
            (name: "b", target: "hero", property: "no.such", keys: Float([(time: 0.0, value: 1.0)])),
        ])"#;
        app.world_mut().spawn((Transform::default(), Name::new("Hero")));
        call(
            &mut app,
            json!({ "action": "load", "seq_ron": seq, "play": true, "bindings": { "hero": { "name": "Hero" } } }),
        )
        .expect("load");
        app.update();
        let out = call(&mut app, json!({})).expect("query");
        let kinds: Vec<&str> = out["players"][0]["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["kind"].as_str())
            .collect();
        assert_eq!(kinds, ["unbound_target", "unknown_property"]);
        let rows = out["players"][0]["bindings"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["target"], "ghost");
        assert_eq!(rows[0]["bound"], false);
    }

    /// `scene.spawn` inserts the ECS `Name`. A `{ "name": .. }` binding must find that entity.
    #[test]
    fn bind_resolves_the_ecs_name_scene_spawn_inserts() {
        let mut app = app();
        let hero = app
            .world_mut()
            .spawn((Transform::default(), regia_app::prelude::Name::new("Hero")))
            .id();
        call(&mut app, json!({ "action": "load", "seq_ron": SEQ })).expect("load");
        let out = call(
            &mut app,
            json!({ "action": "bind", "bindings": { "hero": { "name": "Hero" } } }),
        )
        .expect("bind");
        let rows = out["state"]["bindings"].as_array().expect("rows");
        let row = rows.iter().find(|r| r["target"] == "hero").expect("hero row");
        // The name is stored on this call. The player resolves it on the next tick.
        assert_eq!(row["binding"]["entity"], json!(hero.to_bits()));
        app.update();
        let listed = call(&mut app, json!({})).expect("query");
        let rows = listed["players"][0]["bindings"].as_array().expect("rows");
        let row = rows.iter().find(|r| r["target"] == "hero").expect("hero row");
        assert_eq!(row["bound"], true);
        assert_eq!(row["entity"], json!(hero.to_bits()));
    }

    #[test]
    fn bind_and_unbind_change_only_the_named_targets() {
        let mut app = app();
        let hero = app.world_mut().spawn(Transform::default()).id();
        call(&mut app, json!({ "action": "load", "seq_ron": SEQ })).expect("load");
        let out = call(
            &mut app,
            json!({ "action": "bind", "bindings": { "hero": hero.to_bits(), "extra": "00000000-0000-0000-0000-00000000002a" } }),
        )
        .expect("bind");
        let rows = out["state"]["bindings"].as_array().expect("rows");
        assert_eq!(rows.len(), 2, "a bound name no track uses is listed too");
        assert_eq!(rows[1]["binding"]["entity"], hero.to_bits());
        let out = call(
            &mut app,
            json!({ "action": "bind", "bindings": { "extra": null } }),
        )
        .expect("unbind");
        assert_eq!(out["state"]["bindings"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn oversized_sequences_and_binding_tables_are_refused() {
        let mut app = app();
        let tracks: Vec<Value> = (0..=MAX_TRACKS)
            .map(|i| {
                json!({ "name": format!("t{i}"), "target": "hero", "property": "visible",
                    "keys": { "Bool": [{ "time": 0.0, "value": true }] } })
            })
            .collect();
        let err = call(
            &mut app,
            json!({ "action": "load", "sequence": { "version": 1, "tracks": tracks } }),
        )
        .expect_err("too many tracks");
        assert_eq!(code(err), "too_large");
        let keys: Vec<Value> = (0..=MAX_KEYS)
            .map(|i| json!({ "time": i as f64 * 0.01, "value": 1.0 }))
            .collect();
        let err = call(
            &mut app,
            json!({ "action": "load", "sequence": { "version": 1, "tracks": [
                { "name": "t", "target": "hero", "property": "camera.fov", "keys": { "Float": keys } }
            ] } }),
        )
        .expect_err("too many keys");
        assert_eq!(code(err), "too_large");
        let bindings: Map<String, Value> = (0..=MAX_BINDINGS)
            .map(|i| (format!("b{i}"), Value::Null))
            .collect();
        let err = call(
            &mut app,
            json!({ "action": "load", "seq_ron": SEQ, "bindings": Value::Object(bindings) }),
        )
        .expect_err("too many bindings");
        assert_eq!(code(err), "too_large");
        assert_eq!(call(&mut app, json!({})).expect("list")["count"], 0);
    }

    #[test]
    fn errors_carry_lowercase_codes_and_change_nothing() {
        let mut app = app();
        let c = |app: &mut App, p: Value| code(call(app, p).expect_err("rejected"));
        assert_eq!(c(&mut app, json!({ "action": "dance" })), "bad_action");
        assert_eq!(c(&mut app, json!({ "action": 3 })), "bad_action");
        assert_eq!(c(&mut app, json!({ "action": "play" })), "no_player");
        assert_eq!(c(&mut app, json!({ "action": "load" })), "bad_sequence");
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": "(version: 1, tracks: [(name: \"\")])" })
            ),
            "bad_sequence"
        );
        assert_eq!(
            c(&mut app, json!({ "action": "load", "seq_ron": "(version: 99)" })),
            "bad_sequence"
        );
        assert_eq!(
            c(&mut app, json!({ "action": "load", "shots_ron": "nope" })),
            "bad_shots"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": SEQ, "end": "forever" })
            ),
            "bad_setting"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": SEQ, "blend_in": -1 })
            ),
            "bad_setting"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": SEQ, "camera": "x" })
            ),
            "bad_entity"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": SEQ, "bindings": { "hero": true } })
            ),
            "bad_binding"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": SEQ, "bindings": { "hero": "not-a-guid" } })
            ),
            "bad_binding"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "load", "seq_ron": SEQ, "bindings": { "hero": { "name": "Nobody" } } })
            ),
            "unknown_entity"
        );
        assert_eq!(
            call(&mut app, json!({})).expect("list")["count"],
            0,
            "nothing was spawned"
        );
        let gone = app.world_mut().spawn_empty().id();
        app.world_mut().despawn(gone);
        assert_eq!(
            c(&mut app, json!({ "action": "play", "player": gone.to_bits() })),
            "unknown_player"
        );
        call(&mut app, json!({ "action": "load", "seq_ron": SEQ })).expect("first");
        call(&mut app, json!({ "action": "load", "seq_ron": SEQ })).expect("second");
        assert_eq!(c(&mut app, json!({ "action": "pause" })), "several_players");
        let first = call(&mut app, json!({})).expect("list")["players"][0]["player"].clone();
        assert_eq!(
            c(&mut app, json!({ "action": "seek", "player": first })),
            "bad_time"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "sample", "player": first, "time": 1, "frame": 2 })
            ),
            "bad_time"
        );
        assert_eq!(
            c(&mut app, json!({ "action": "seek", "player": first, "time": -1 })),
            "bad_time"
        );
    }

    const FX: &str = r#"(version: 2, tracks: [(name: "slide", target: "lamp", property: "transform.position",
        keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0), ease: Bezier, tangent: Linear),
            (time: 2.0, value: (4.0, 0.0, 0.0), tangent: Linear)]))],
        markers: [(name: "flash", time: 1.0)])"#;

    const PARENT: &str = r#"(version: 2, tracks: [
        (name: "fx", keys: Sub([(sequence: "fx.seq.ron", start: 1.0, end: 3.0, bindings: {"lamp": "hero"})])),
        (name: "body", target: "hero", keys: Clip([(clip: "wave", start: 0.0, end: 2.0, blend_out: 1.0)])),
        (name: "music", keys: Audio([(sound: "theme", start: 0.5, end: 2.5, offset: 0.25)])),
    ])"#;

    #[test]
    fn library_sub_sequences_clips_and_sounds_show_in_sample_and_state() {
        let mut app = app();
        let hero = app
            .world_mut()
            .spawn((Transform::default(), Name::new("Hero")))
            .id();
        let listed = call(
            &mut app,
            json!({ "action": "library", "name": "fx.seq.ron", "seq_ron": FX }),
        )
        .expect("library add");
        assert_eq!(listed["sequences"], json!(["fx.seq.ron"]));
        let clip =
            serde_json::to_value(regia_animation::AnimationClip::empty("wave", 1.0)).expect("clip json");
        let clips = call(
            &mut app,
            json!({ "action": "clip", "name": "wave", "clip": clip }),
        )
        .expect("clip add");
        assert_eq!(clips["clips"], json!(["wave"]));
        let out = call(
            &mut app,
            json!({ "action": "load", "seq_ron": PARENT, "bindings": { "hero": { "name": "Hero" } }, "play": true }),
        )
        .expect("load");
        for _ in 0..8 {
            app.update();
        }
        let sample = call(&mut app, json!({ "action": "sample", "time": 2.0 })).expect("sample");
        let values = &sample["sample"]["values"];
        assert_eq!(values[0]["target"], json!("hero"), "{sample}");
        assert_eq!(values[0]["depth"], json!(1));
        let x = values[0]["value"][0].as_f64().expect("x");
        assert!(
            (x - 2.0).abs() < 1.0e-5,
            "linear tangents: a straight Bezier, {x}"
        );
        assert_eq!(
            sample["sample"]["clips"],
            json!([]),
            "blended out at its end: {sample}"
        );
        let at_one = call(&mut app, json!({ "action": "sample", "time": 1.5 })).expect("sample");
        assert_eq!(
            at_one["sample"]["clips"][0]["blends"][0]["clip"],
            json!("wave"),
            "{at_one}"
        );
        assert_eq!(
            at_one["sample"]["clips"][0]["blends"][0]["weight"],
            json!(0.5),
            "{at_one}"
        );
        assert_eq!(at_one["sample"]["sounds"][0]["sound"], json!("theme"));
        assert_eq!(at_one["sample"]["sounds"][0]["position"], json!(1.25));
        let state = &out["state"];
        assert_eq!(
            state["version"],
            json!(regia_framework::sequencer::SEQUENCE_FORMAT_VERSION),
            "a v2 file loads as the current version"
        );
        assert_eq!(state["sub_sequences"], json!(["fx.seq.ron"]));
        assert_eq!(state["track_kinds"]["clip"], json!(1));
        let query = call(&mut app, json!({ "action": "query" })).expect("query");
        let player = &query["players"][0];
        assert_eq!(player["targets"], json!(["hero"]), "{player}");
        assert_eq!(player["flat_markers"], json!(1), "the sub-sequence's marker");
        let x = app.world().get::<Transform>(hero).expect("hero").translation.x;
        assert!(
            (x - 2.0).abs() < 1.0e-4,
            "t = 2.0 drives the sub-sequence track: {x}"
        );
        // Removing and listing.
        let removed = call(
            &mut app,
            json!({ "action": "library", "name": "fx.seq.ron", "remove": true }),
        )
        .expect("remove");
        assert_eq!(removed["removed"], json!(true));
        assert_eq!(removed["sequences"], json!([]));
        let listed = call(&mut app, json!({ "action": "clip" })).expect("list");
        assert_eq!(listed["clips"], json!(["wave"]));
    }

    #[test]
    fn library_and_clip_refuse_with_their_codes() {
        let mut app = app();
        let c = |app: &mut App, params: Value| code(call(app, params).expect_err("refused"));
        assert_eq!(
            c(&mut app, json!({ "action": "library", "seq_ron": FX })),
            "bad_name"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "library", "name": "" , "seq_ron": FX })
            ),
            "bad_name"
        );
        assert_eq!(
            c(&mut app, json!({ "action": "library", "name": "x" })),
            "bad_sequence"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "library", "name": "x", "seq_ron": "(version: 9)" })
            ),
            "bad_sequence"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "library", "name": "x", "seq_ron": FX, "remove": true })
            ),
            "bad_sequence"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "clip", "name": "w", "clip": { "nope": 1 } })
            ),
            "bad_clip"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "clip", "clip": serde_json::to_value(regia_animation::AnimationClip::empty("w", 1.0)).expect("json") })
            ),
            "bad_name"
        );
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "clip", "name": "w", "asset": "anim/w.glb" })
            ),
            "bad_clip",
            "no asset server"
        );
        let mut neg = regia_animation::AnimationClip::empty("w", 1.0);
        neg.duration = -1.0;
        assert_eq!(
            c(
                &mut app,
                json!({ "action": "clip", "name": "w", "clip": serde_json::to_value(neg).expect("json") })
            ),
            "bad_clip"
        );
        // A sub-sequence missing from the library is an issue on the player, not an error.
        let out = call(
            &mut app,
            json!({ "action": "load", "seq_ron": PARENT, "play": true }),
        )
        .expect("load");
        app.update();
        let state = call(&mut app, json!({ "action": "query", "player": out["player"] })).expect("query");
        let kinds: Vec<&str> = state["state"]["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"sub_sequence"), "{state}");
    }

    const LOOP_FX: &str = r#"(version: 3, tracks: [
        (name: "glow", target: "lamp", property: "intensity",
            keys: Float([(time: 0.0, value: 0.0), (time: 2.0, value: 1.0)])),
    ], markers: [(name: "tick", time: 0.5)])"#;

    const V3: &str = r#"(version: 3, tracks: [
        (name: "move", target: "hero", property: "transform.position",
            keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 4.0, value: (4.0, 0.0, 0.0))])),
        (name: "steps", keys: Audio([
            (sound: "step", start: 0.0, end: 4.0, time_scale: 2.0,
                spatial: Some((source: Binding("hero"), max_distance: 20.0))),
            (sound: "bell", start: 0.0, end: 4.0, spatial: Some((source: Position((0.0, 0.0, -5.0))))),
        ])),
        (name: "fx", keys: Sub([(sequence: "fx.seq.ron", start: 1.0, end: 4.0, speed: 2.0, repeat: UntilStop)])),
    ])"#;

    const V5: &str = r#"(version: 5, tracks: [
        (name: "steps", keys: Audio([
            (sound: "step", start: 0.0, end: 4.0,
                spatial: Some((source: Position((0.0, 0.0, -5.0)), doppler: 0.5, occlusion: true))),
        ])),
        (name: "line", keys: Audio([
            (sound: "line", start: 0.0, end: 4.0, time_scale: 0.75, time_scale_mode: PreservePitch),
        ])),
    ], markers: [(name: "end", time: 4.0)])"#;

    /// `sequencer.spatial_voices` reports the v5 fields (ADR-3197): Doppler factor, occlusion and
    /// time scale mode per section, flat sections with `all`, the stretch cache counters; a v3 file
    /// reads Doppler 0 after its migration; `all` must be a boolean.
    #[test]
    fn spatial_voices_reports_the_v5_doppler_occlusion_and_stretch_fields() {
        let mut fresh = app();
        call(
            &mut fresh,
            json!({ "action": "load", "seq_ron": V5, "play": true }),
        )
        .expect("load");
        fresh.update();
        let world = fresh.world_mut();
        let spatial_only = spatial_voices(world, &json!({})).expect("voices");
        let list = spatial_only["voices"].as_array().expect("list");
        assert_eq!(list.len(), 1, "{spatial_only}");
        assert_eq!(list[0]["doppler_factor"], json!(0.5));
        assert_eq!(list[0]["occlusion"], json!(true));
        assert_eq!(list[0]["time_scale_mode"], json!("varispeed"));
        assert!(spatial_only["stretch_cache"].is_object(), "{spatial_only}");
        let every = spatial_voices(world, &json!({ "all": true })).expect("all");
        let list = every["voices"].as_array().expect("list");
        assert_eq!(list.len(), 2, "{every}");
        assert_eq!(list[1]["spatial"], json!(false));
        assert_eq!(list[1]["time_scale_mode"], json!("preserve_pitch"));
        assert_eq!(list[1]["rate"], json!(0.75));
        assert!(list[1].get("doppler_factor").is_none());
        let bad = spatial_voices(world, &json!({ "all": "yes" })).expect_err("bad all");
        assert_eq!(
            bad.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("bad_setting")
        );
        // A v3 file migrates with Doppler off.
        let mut old = app();
        old.world_mut().spawn((Transform::default(), Name::new("Hero")));
        call(
            &mut old,
            json!({ "action": "library", "name": "fx.seq.ron", "seq_ron": LOOP_FX }),
        )
        .expect("library");
        call(
            &mut old,
            json!({ "action": "load", "seq_ron": V3, "bindings": { "hero": { "name": "Hero" } }, "play": true }),
        )
        .expect("load v3");
        old.update();
        let voices = spatial_voices(old.world_mut(), &json!({})).expect("voices");
        assert_eq!(voices["voices"][0]["doppler_factor"], json!(0.0), "{voices}");
    }

    #[test]
    fn subtitle_methods_report_cues_and_set_and_read_the_settings() {
        let mut app = app();
        call(
            &mut app,
            json!({ "action": "load", "seq_ron": SEQ, "play": true }),
        )
        .expect("load");
        app.update();
        let cues = subtitles(app.world_mut(), &json!({})).expect("subtitles");
        assert_eq!(cues["cues"], json!([]), "{cues}");
        assert_eq!(cues["status"], "playing");
        assert_eq!(cues["settings"]["enabled"], json!(true));
        assert_eq!(cues["settings"]["burn_in"], json!(false), "off by default");
        let set = subtitle_settings(
            app.world_mut(),
            &json!({ "burn_in": true, "opt_out_tracks": ["sfx"], "opt_out_sounds": [] }),
        )
        .expect("settings");
        assert_eq!(set["burn_in"], json!(true));
        assert_eq!(set["opt_out_tracks"], json!(["sfx"]));
        let read = subtitle_settings(app.world_mut(), &json!({})).expect("read");
        assert_eq!(read, set, "no parameters only reads");
        for bad in [
            json!({ "enabled": "yes" }),
            json!({ "opt_out_tracks": "sfx" }),
            json!({ "opt_out_sounds": [1] }),
        ] {
            let e = subtitle_settings(app.world_mut(), &bad).expect_err("refused");
            assert_eq!(code(e), "bad_setting");
        }
        let after = subtitle_settings(app.world_mut(), &json!({})).expect("read");
        assert_eq!(after, set, "a refused call changes nothing");
        let e = subtitles(app.world_mut(), &json!({ "player": "x" })).expect_err("bad entity");
        assert_eq!(code(e), "bad_entity");
    }

    #[test]
    fn sequencer_v3_methods_report_spatial_voices_and_loops_and_rename_bindings() {
        let mut app = app();
        let hero = app
            .world_mut()
            .spawn((Transform::default(), Name::new("Hero")))
            .id();
        call(
            &mut app,
            json!({ "action": "library", "name": "fx.seq.ron", "seq_ron": LOOP_FX }),
        )
        .expect("library");
        call(
            &mut app,
            json!({ "action": "load", "seq_ron": V3, "bindings": { "hero": { "name": "Hero" } }, "play": true }),
        )
        .expect("load");
        for _ in 0..7 {
            app.update();
        }
        let v = |app: &mut App, method: &str, params: Value| {
            let world = app.world_mut();
            match method {
                "sequencer.spatial_voices" => spatial_voices(world, &params),
                "sequencer.sub_loops" => sub_loops(world, &params),
                _ => rename_binding_method(world, &params),
            }
        };
        let voices = v(&mut app, "sequencer.spatial_voices", json!({})).expect("voices");
        let list = voices["voices"].as_array().expect("list");
        assert_eq!(list.len(), 2, "{voices}");
        assert_eq!(list[0]["source"], json!({ "binding": "hero" }));
        assert_eq!(list[0]["bound_entity"], json!(hero.to_bits()));
        assert_eq!(list[0]["rate"], json!(2.0));
        assert_eq!(list[0]["max_distance"], json!(20.0));
        assert_eq!(list[1]["source"], json!({ "position": [0.0, 0.0, -5.0] }));
        assert_eq!(list[1]["voice"], Value::Null, "no Audio resource: no voice");
        // Loops: speed 2 over a 2 s child is a 1 s pass; at 2.75 the pass is 1, child time 1.5.
        let loops = v(&mut app, "sequencer.sub_loops", json!({ "time": 2.75 })).expect("loops");
        let sections = loops["sections"].as_array().expect("sections");
        assert_eq!(sections.len(), 1, "{loops}");
        assert_eq!(sections[0]["repeat"], "until_stop");
        assert_eq!(
            (sections[0]["pass"].as_u64(), sections[0]["local_time"].as_f64()),
            (Some(1), Some(1.5))
        );
        assert_eq!(sections[0]["pass_length"], json!(2.0));
        let sample = call(&mut app, json!({ "action": "sample", "time": 2.75 })).expect("sample");
        assert_eq!(sample["sample"]["sounds"][0]["emitter"], "hero");
        // Rename: tracks and the spatial sound move, and the entity stays bound.
        let out = v(
            &mut app,
            "sequencer.rename_binding",
            json!({ "from": "hero", "to": "guard" }),
        )
        .expect("rename");
        assert_eq!(
            (out["tracks"].as_u64(), out["sounds"].as_u64()),
            (Some(1), Some(1))
        );
        app.update();
        let state = call(&mut app, json!({ "action": "query", "player": out["player"] })).expect("query");
        assert_eq!(state["state"]["targets"], json!(["guard", "lamp"]), "{state}");
        let voices = v(&mut app, "sequencer.spatial_voices", json!({})).expect("voices");
        assert_eq!(voices["voices"][0]["bound_entity"], json!(hero.to_bits()));
        // Refusals.
        for (params, expected) in [
            (json!({ "from": "guard", "to": "lamp" }), "seq_binding_in_use"),
            (json!({ "from": "nobody", "to": "x" }), "seq_binding_unknown"),
            (json!({ "from": "guard", "to": "guard" }), "seq_binding_same"),
            (json!({ "from": "guard" }), "bad_name"),
            (json!({ "from": "guard", "to": "x", "player": "p" }), "bad_entity"),
        ] {
            assert_eq!(
                code(v(&mut app, "sequencer.rename_binding", params).expect_err("refused")),
                expected
            );
        }
        assert_eq!(
            code(v(&mut app, "sequencer.sub_loops", json!({ "time": -1.0 })).expect_err("negative")),
            "bad_setting"
        );
    }

    /// ADR-2862: the method lists the sections, the frame each session shows and what went wrong.
    #[test]
    fn video_sections_reports_the_frame_shown_the_drift_and_a_missing_source() {
        use regia_media::mux::PatternClip;
        use regia_media::plugin::MediaFiles;

        let v = |app: &mut App, method: &str, params: Value| {
            app.update();
            let world = app.world_mut();
            match method {
                "sequencer.video_sections" => video_sections(world, &params),
                "sequencer.rename_binding" => rename_binding_method(world, &params),
                _ => unreachable!(),
            }
        };
        let mut app = app();
        let mut files = MediaFiles::default();
        files.insert("movies/a.webm", PatternClip::new(16, 8, 120, 60).to_webm());
        app.insert_resource(files).add_plugins(regia_media::MediaPlugin);
        let seq = r#"(version: 4, tracks: [
            (name: "wall", keys: Video([
                (video: "movies/a.webm", start: 0.5, end: 2.0, offset: 0.25, volume_db: -6.0,
                    target: Fullscreen(layer: -1, fit: Fill)),
                (video: "movies/gone.webm", start: 0.0, end: 2.0, looping: true),
                (video: "movies/a.webm", start: 2.0, end: 3.0, target: Screen(binding: "tv", slot: Albedo)),
            ])),
        ])"#;
        call(
            &mut app,
            json!({ "action": "load", "seq_ron": seq, "clock": "external", "play": true }),
        )
        .expect("load");
        call(&mut app, json!({ "action": "seek", "time": 1.0 })).expect("seek");
        app.update();
        let out = v(&mut app, "sequencer.video_sections", json!({})).expect("sections");
        assert_eq!(out["time"], json!(1.0), "{out}");
        assert_eq!(out["frame_exact"], json!(true), "the tests' clock is manual");
        let sections = out["sections"].as_array().expect("sections");
        assert_eq!(sections.len(), 3, "{out}");
        assert_eq!(sections[0]["video"], "movies/a.webm");
        assert_eq!(sections[0]["offset"], json!(0.25));
        assert_eq!(sections[0]["volume_db"], json!(-6.0));
        assert_eq!(sections[0]["target"]["kind"], "fullscreen");
        assert_eq!(sections[0]["target"]["layer"], json!(-1));
        assert_eq!(sections[0]["target"]["fit"], "Fill");
        assert_eq!(sections[0]["in_effect"], json!(true));
        assert_eq!(sections[1]["looping"], json!(true));
        assert_eq!(sections[2]["target"]["binding"], "tv");
        assert_eq!(sections[2]["target"]["slot"], "Albedo");
        assert_eq!(sections[2]["in_effect"], json!(false));
        let active = out["active"].as_array().expect("active");
        assert_eq!(active.len(), 2, "{out}");
        let playing = active
            .iter()
            .find(|a| a["video"] == "movies/a.webm")
            .expect("the session");
        assert_eq!(
            playing["frame_shown"],
            json!(45),
            "0.75 s into a 60 fps clip: {out}"
        );
        assert_eq!(playing["drift"], json!(0.0));
        assert_eq!(playing["certain"], json!(true));
        assert_eq!(playing["exact"], json!(true));
        assert_eq!(playing["state"], "playing");
        assert_eq!(playing["frames_dropped"], json!(0));
        assert!(playing["media"].is_u64());
        let gone = active
            .iter()
            .find(|a| a["video"] == "movies/gone.webm")
            .expect("the missing section");
        assert_eq!(gone["error"], "source_not_found");
        assert_eq!(gone["media"], Value::Null);
        assert_eq!(gone["frame_shown"], Value::Null);
        let kinds: Vec<&str> = out["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|i| i["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"seq_video_missing"), "{out}");
        assert_eq!(out["opened"], json!(2));
        // Rename reaches the section's target.
        let renamed = v(
            &mut app,
            "sequencer.rename_binding",
            json!({ "from": "tv", "to": "wall_screen" }),
        )
        .expect("rename");
        assert_eq!(renamed["videos"], json!(1));
        // `sample` and the state see the sections too, at any time, without moving the clock.
        let sampled = call(&mut app, json!({ "action": "sample", "time": 2.5 })).expect("sample");
        assert_eq!(sampled["sample"]["videos"][0]["target"], "screen");
        assert_eq!(sampled["sample"]["videos"][0]["binding"], "wall_screen");
        assert_eq!(sampled["sample"]["videos"][0]["position"], json!(0.5));
        let state = call(
            &mut app,
            json!({ "action": "query", "player": renamed["player"] }),
        )
        .expect("query");
        assert_eq!(state["state"]["track_kinds"]["video"], json!(1), "{state}");
        assert_eq!(
            code(v(&mut app, "sequencer.video_sections", json!({ "player": "p" })).expect_err("refused")),
            "bad_entity"
        );
    }

    #[test]
    fn sub_loops_answers_seq_loop_too_deep_for_a_deep_loop_tree() {
        let mut app = app();
        // l0 loops l1 loops ... l4 loops the leaf: five looping levels.
        let looped = |next: &str| {
            format!(
                r#"(version: 3, tracks: [(name: "s", keys: Sub([(sequence: "{next}", start: 0.0, end: 4.0, repeat: UntilStop)]))])"#
            )
        };
        call(
            &mut app,
            json!({ "action": "library", "name": "leaf", "seq_ron": LOOP_FX }),
        )
        .expect("leaf");
        for i in 0..5 {
            let next = if i == 4 {
                "leaf".to_string()
            } else {
                format!("l{}", i + 1)
            };
            call(
                &mut app,
                json!({ "action": "library", "name": format!("l{i}"), "seq_ron": looped(&next) }),
            )
            .expect("level");
        }
        let root =
            r#"(version: 3, tracks: [(name: "s", keys: Sub([(sequence: "l0", start: 0.0, end: 4.0)]))])"#;
        call(
            &mut app,
            json!({ "action": "load", "seq_ron": root, "play": true }),
        )
        .expect("load");
        app.update();
        let err = sub_loops(app.world_mut(), &json!({})).expect_err("too deep");
        assert_eq!(code(err), "seq_loop_too_deep");
        // The player reports the same problem as an issue.
        let state = call(&mut app, json!({ "action": "query" })).expect("query");
        let issues = state["players"][0]["issues"].to_string();
        assert!(issues.contains("looping sub-sequences nest 5 deep"), "{issues}");
    }

    // -- Take recorder (ADR-2637) --

    fn record_app(dir: &std::path::Path) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SequencerPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(
                16_666_667,
            )))
            .insert_resource(crate::AgentSettings {
                project_dir: Some(dir.to_path_buf()),
            });
        app
    }

    #[test]
    fn record_start_stop_and_state_write_a_take_next_to_the_source() {
        let dir = regia_test_utils::unique_temp_dir("agent-take");
        let mut app = record_app(&dir);
        let hero = app.world_mut().spawn(Transform::default()).id();
        let still = app.world_mut().spawn(Transform::default()).id();
        let state = record_start(
            app.world_mut(),
            &json!({
                "bindings": [
                    { "name": "hero", "entity": hero.to_bits(), "channels": ["transform"] },
                    { "name": "still", "entity": still.to_bits() },
                ],
                "countdown": 2,
            }),
        )
        .expect("start");
        assert_eq!(state["status"], "countdown");
        assert_eq!(state["countdown_left"], 2);
        assert_eq!(
            code(
                record_start(
                    app.world_mut(),
                    &json!({ "bindings": [{ "name": "x", "camera": true }] })
                )
                .expect_err("refused")
            ),
            "take_busy"
        );
        for k in 0..40 {
            app.world_mut()
                .get_mut::<Transform>(hero)
                .expect("hero")
                .translation
                .x = k as f32 * 0.1;
            app.update();
        }
        let state = record_state(app.world_mut(), &json!({})).expect("state");
        assert_eq!(state["status"], "recording");
        assert!(state["frames"].as_u64().expect("frames") >= 37);
        assert_eq!(state["bindings"][0]["name"], "hero");
        let done = record_stop(
            app.world_mut(),
            &json!({ "sequence": "cine/run.seq.ron", "at": 1.0 }),
        )
        .expect("stop");
        assert_eq!(done["take"], "cine/run_take1.seq.ron");
        assert_eq!(done["section_track"], "take1");
        assert_eq!(done["created_sequence"], true);
        let tracks = done["tracks"].as_array().expect("tracks");
        let still_position = tracks
            .iter()
            .find(|t| t["name"] == "still/transform.position")
            .expect("still track");
        assert_eq!(still_position["keys"], 2, "a still object writes two keys");
        let text = std::fs::read_to_string(dir.join("cine/run_take1.seq.ron")).expect("take file");
        let take = Sequence::from_ron(&text).expect("take loads");
        assert_eq!(take.name, "run_take1");
        let source =
            Sequence::from_ron(&std::fs::read_to_string(dir.join("cine/run.seq.ron")).expect("source"))
                .expect("source loads");
        assert_eq!(source.tracks[0].name, "take1");
        // The library learned the take, so the source plays it at once.
        assert!(
            app.world()
                .resource::<SequenceLibrary>()
                .get("cine/run_take1.seq.ron")
                .is_some()
        );
        assert_eq!(
            code(record_stop(app.world_mut(), &json!({})).expect_err("refused")),
            "take_not_recording"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_errors_carry_codes_and_a_failed_save_keeps_the_take() {
        let dir = regia_test_utils::unique_temp_dir("agent-take-errors");
        let mut app = record_app(&dir);
        let hero = app.world_mut().spawn(Transform::default()).id();
        let start =
            |app: &mut App, params: Value| code(record_start(app.world_mut(), &params).expect_err("refused"));
        assert_eq!(start(&mut app, json!({})), "take_no_bindings");
        assert_eq!(
            start(&mut app, json!({ "bindings": [{ "entity": hero.to_bits() }] })),
            "take_bad_binding"
        );
        assert_eq!(
            start(
                &mut app,
                json!({ "bindings": [{ "name": "h", "entity": hero.to_bits(), "camera": true }] })
            ),
            "take_bad_binding"
        );
        assert_eq!(
            start(
                &mut app,
                json!({ "bindings": [{ "name": "h", "entity": hero.to_bits(), "channels": ["wings"] }] })
            ),
            "take_bad_channel"
        );
        let gone = app.world_mut().spawn_empty().id();
        app.world_mut().despawn(gone);
        assert_eq!(
            start(
                &mut app,
                json!({ "bindings": [{ "name": "h", "entity": gone.to_bits() }] })
            ),
            "unknown_entity"
        );
        assert_eq!(
            start(
                &mut app,
                json!({ "bindings": [{ "name": "h", "entity": hero.to_bits() }], "tolerance": { "position": -1 } })
            ),
            "take_bad_tolerance"
        );
        assert_eq!(
            code(record_stop(app.world_mut(), &json!({})).expect_err("refused")),
            "take_not_recording"
        );

        record_start(
            app.world_mut(),
            &json!({ "bindings": [{ "name": "h", "entity": hero.to_bits() }] }),
        )
        .expect("start");
        for _ in 0..5 {
            app.update();
        }
        std::fs::create_dir_all(dir.join("cine")).expect("dir");
        std::fs::write(dir.join("cine/broken.seq.ron"), "(version: 3, tracks: [(").expect("write");
        assert_eq!(
            code(
                record_stop(app.world_mut(), &json!({ "sequence": "cine/broken.seq.ron" }))
                    .expect_err("refused")
            ),
            "take_source_invalid"
        );
        assert_eq!(
            record_state(app.world_mut(), &json!({})).expect("state")["unsaved_take"],
            true
        );
        assert_eq!(
            code(
                record_start(
                    app.world_mut(),
                    &json!({ "bindings": [{ "name": "h", "entity": hero.to_bits() }] })
                )
                .expect_err("refused")
            ),
            "take_unsaved",
            "a kept take blocks the next one"
        );
        assert_eq!(
            code(
                record_stop(app.world_mut(), &json!({ "sequence": "../out.seq.ron" })).expect_err("refused")
            ),
            "take_bad_path"
        );
        // The kept take saves on the next call; without a path it comes back as text.
        let text = record_stop(app.world_mut(), &json!({})).expect("kept take")["seq_ron"]
            .as_str()
            .expect("seq_ron")
            .to_string();
        assert!(Sequence::from_ron(&text).is_ok());
        assert_eq!(
            record_state(app.world_mut(), &json!({})).expect("state")["unsaved_take"],
            false
        );

        record_start(
            app.world_mut(),
            &json!({ "bindings": [{ "name": "h", "entity": hero.to_bits() }] }),
        )
        .expect("start");
        assert_eq!(
            record_stop(app.world_mut(), &json!({ "cancel": true })).expect("cancel")["cancelled"],
            true
        );
        assert_eq!(
            record_state(app.world_mut(), &json!({})).expect("state")["status"],
            "idle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-5957: `sequence.record_state` is a `read` method, so it must change nothing another call
    /// can observe. It used to insert a `TakeRecorder` into an app that had none.
    #[test]
    fn record_state_reads_without_creating_a_recorder() {
        let mut world = World::new();
        let state = record_state(&world, &json!({})).expect("state");
        assert_eq!(state["status"], "idle");
        assert_eq!(state["frames"], 0);
        assert_eq!(state["unsaved_take"], false);
        assert_eq!(
            state,
            take_state_json(&TakeRecorder::default()),
            "an app without a recorder reads as an idle one"
        );
        assert!(
            !world.contains_resource::<TakeRecorder>(),
            "record_state inserted a recorder"
        );
        // With a recorder it reads that one, and leaves it unchanged.
        world.init_resource::<TakeRecorder>();
        world.clear_trackers();
        let again = record_state(&world, &json!({})).expect("state");
        assert_eq!(again, state);
        assert!(!world.resource_ref::<TakeRecorder>().is_changed());
        let schema = crate::schema::method("sequence.record_state").expect("schema");
        assert_eq!(schema.permission, crate::schema::Permission::Read);
        assert!(!schema.mutating);
        assert!(schema.params.is_empty(), "record_state takes no path");
    }

    #[test]
    fn record_methods_are_registered_with_contracts_and_schemas() {
        for method in [
            "sequence.record_start",
            "sequence.record_stop",
            "sequence.record_state",
        ] {
            assert!(crate::schema::method(method).is_some(), "{method} has a schema");
            assert!(
                crate::contract::CONTRACTS.iter().any(|c| c.method == method),
                "{method} has a contract"
            );
        }
    }
}
