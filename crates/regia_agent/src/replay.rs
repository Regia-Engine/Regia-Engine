// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for DO27 replay playback (`docs/specs/25-sim-bridge-and-do27.md`,
//! `book/agents/replay.md`): `replay.load`, `replay.seek`, `replay.step`, `replay.play`,
//! `replay.pause`, `moments.list`, `moments.get`.
//!
//! They drive the world's [`ReplaySim`] resource, which is the same object a
//! `SimBridgePlugin<ReplaySim>` renders from, so a seek or step is visible in the next frame's
//! screenshot. They also work headless: with no resource, `replay.load` inserts a default
//! (opaque-codec) player and every call afterwards answers from the file alone.
//!
//! `replay.play` (ADR-1519, ADR-1524) plays on the simulation clock (`Time::sim_delta`): the
//! bridge advances the player by each frame's simulation delta, and `drive_unbridged_replay` does
//! the same for a player that is only a resource. Under a render-out with a `time_map` that delta is
//! the job's simulation step (`SimStep`), so the replay's `speed` composes with the map: a 2x
//! replay under a 0.25 map plays at 0.5x of the output's real time. With `start: "render_out"` the
//! player waits at its tick and starts on the next render-out's frame 0 (`start_armed_replay`), so
//! the delivered frames are at `from_tick + tick_rate · speed · sim_time` exactly. Sub-tick
//! blending is presentation only: `replay.seek`/`replay.step` snapshots are exact unless
//! `replay.step` is asked for the `presented` pose.

use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::replay_sim::{
    MAX_BLEND_GAP_TICKS, Moment, MomentKind, MomentQuery, ReplayError, ReplayEvent, ReplayFile, ReplaySim,
    ReplaySnapshot, role_name,
};
use regia_framework::sim_bridge::SimEvent;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Entities listed in a snapshot summary before it says `truncated`.
const SNAPSHOT_ENTITY_CAP: usize = 100;
/// Default and maximum page size of `moments.list`.
const LIST_DEFAULT: usize = 50;
const LIST_MAX: usize = 1000;
/// Most ticks one `replay.step` advances (ten minutes at 60 Hz); more is clamped and flagged.
const STEP_TICKS_MAX: u32 = 36_000;
/// Most events one `replay.step` serializes; the rest are counted in `events_total`.
const STEP_EVENTS_MAX: usize = 1000;
/// Slowest and fastest `replay.play` speed (1 = real time; the player counts thousandths).
const MIN_SPEED: f64 = 0.001;
const MAX_SPEED: f64 = 64.0;

fn replay_error(error: &ReplayError) -> RpcError {
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        "F0002",
        format!("replay.load: {error} [{}]", error.code()),
        "the file must be a DO27 replay, version 2, 3 or 4 (docs/formats/do27replay.md); a version 2 file with a full record loads only into a ReplaySim whose FullCodec knows the writing build's layout (FullCodec::v2_full_len); otherwise convert it to version 3",
    )
}

fn not_loaded() -> RpcError {
    RpcError::not_found(
        "no replay is loaded",
        "call replay.load { path } first (it creates the ReplaySim player when the app has none)",
    )
}

/// Runs `f` on the loaded player, or `NOT_FOUND` when nothing is loaded.
fn with_loaded<R>(
    world: &mut World,
    f: impl FnOnce(&mut ReplaySim) -> Result<R, RpcError>,
) -> Result<R, RpcError> {
    let Some(mut sim) = world.get_resource_mut::<ReplaySim>() else {
        return Err(not_loaded());
    };
    if sim.file().is_none() {
        return Err(not_loaded());
    }
    f(&mut sim)
}

fn opt_u64(params: &Value, name: &str) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a non-negative integer"))),
    }
}

fn opt_u32(params: &Value, name: &str) -> Result<Option<u32>, RpcError> {
    match opt_u64(params, name)? {
        None => Ok(None),
        Some(v) => u32::try_from(v)
            .map(Some)
            .map_err(|_| RpcError::invalid_params(format!("`{name}` is above {}", u32::MAX))),
    }
}

fn vec3_json(v: regia_core::math::Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn moment_json(index: Option<usize>, m: &Moment) -> Value {
    json!({
        "index": index,
        "tick": m.tick,
        "drained_tick": m.drained_tick,
        "kind": m.kind.name(),
        "kind_wire": m.kind.to_wire(),
        "span": { "match": m.span_match, "round": m.span_round, "fight": m.span_fight },
        "participants": m.participants.iter()
            .map(|p| json!({ "role": p.role, "entity_id": p.entity_id }))
            .collect::<Vec<_>>(),
        "position": vec3_json(m.position),
        "damage": m.damage,
        "weapon": m.weapon,
        "attacker": m.attacker,
        "victim": m.victim,
        "importance": m.importance,
    })
}

fn event_json(event: &ReplayEvent) -> Value {
    match event {
        ReplayEvent::Moment(m) => {
            json!({ "type": "moment", "tick": m.drained_tick, "moment": moment_json(None, m) })
        }
        ReplayEvent::Command { tick, command } => json!({
            "type": "command", "tick": tick, "peer_id": command.peer_id, "ok": command.ok,
            "kind": command.kind, "text": command.text,
        }),
        ReplayEvent::Abort(a) => json!({ "type": "abort", "tick": a.tick, "reason": a.reason }),
    }
}

fn snapshot_json(snap: &ReplaySnapshot) -> Value {
    let entities: Vec<Value> = snap
        .entities
        .iter()
        .take(SNAPSHOT_ENTITY_CAP)
        .map(|e| {
            json!({
                "id": e.id, "kind": e.kind.as_ref(), "team": e.team,
                "position": vec3_json(e.translation),
                "rotation": [e.rotation.x, e.rotation.y, e.rotation.z, e.rotation.w],
            })
        })
        .collect();
    json!({
        "tick": snap.tick,
        "full_tick": snap.full_tick,
        "nav_tick": snap.nav_tick,
        "entity_count": snap.entities.len(),
        "entities": entities,
        "truncated": snap.entities.len() > SNAPSHOT_ENTITY_CAP,
        "decode_error": snap.decode_error,
        "blend": snap.blend.map(|b| json!({ "next_full_tick": b.next_full_tick, "alpha": b.alpha })),
    })
}

fn position_json(sim: &ReplaySim) -> Value {
    let rate = sim.file().map_or(0, |f| f.header.tick_rate);
    json!({
        "tick": sim.tick(),
        "end_tick": sim.end_tick(),
        "seconds": if rate == 0 { 0.0 } else { f64::from(sim.tick()) / f64::from(rate) },
        "at_end": sim.tick() >= sim.end_tick(),
        "tick_fraction": sim.tick_fraction(),
        "playing": sim.playing(),
        "speed": f64::from(sim.speed_permille()) / 1000.0,
    })
}

fn header_json(file: &ReplayFile) -> Value {
    let h = &file.header;
    json!({
        "version": h.version,
        "protocol": h.protocol,
        "tick_rate": h.tick_rate,
        "lobby_preset": h.lobby_preset,
        "seed": h.seed.to_string(),
        "bootstrap": h.bootstrap,
        "map": h.map,
        "mode": h.mode,
        "teams": h.teams,
        "input_delay": h.input_delay,
        "sim_git_sha": h.sim_git_sha,
        "sim_abi": h.sim_abi,
        "content_hash": h.content_hash,
        "config_hash": h.config_hash,
        "players": h.players.iter().map(|p| json!({
            "peer_id": p.peer_id, "account_id": p.account_id, "callsign": p.callsign,
            "role": role_name(p.role), "team": p.team,
        })).collect::<Vec<_>>(),
        "peers": h.peers.iter().map(|p| json!({ "peer_id": p.peer_id, "role": role_name(p.role) })).collect::<Vec<_>>(),
    })
}

/// Extra directories `replay.load` may read a `.do27replay` from (ADR-1965): a replay store outside
/// the project. They add to the project root, the system temp dir, `output_roots`
/// and `replay_roots` of `.regia/agent.toml`. Reading only: no method writes under them (a store is
/// not an output root). A host inserts it, for example from a `--replay-root DIR` flag; the paths
/// should be absolute (a relative one resolves against the process's working dir).
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayRoots(pub Vec<std::path::PathBuf>);

/// The kinds of directory a `replay.load` path may lie under (`data.allowed_roots` of a refusal).
const REPLAY_ROOT_KINDS: &[&str] = &["project", "temp_dir", "output_roots", "replay_roots"];

/// Confines `replay.load`'s `path` (ADR-1769, ADR-1965): relative to the project root, or absolute
/// under the project root, the system temp dir, an `output_roots` or `replay_roots`
/// entry of `.regia/agent.toml`, or a host [`ReplayRoots`] entry; no `..`, no NUL, no symlink
/// leading out of every one of them.
fn confine_replay_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    let root = crate::req_path::project_root(world);
    let mut extra = Vec::new();
    if let Some(permissions) = world.get_resource::<crate::safety::AgentPermissions>() {
        extra.extend(permissions.config.output_roots.iter().cloned());
        extra.extend(permissions.config.replay_roots.iter().cloned());
    }
    if let Some(roots) = world.get_resource::<ReplayRoots>() {
        extra.extend(roots.0.iter().cloned());
    }
    crate::render_out::resolve_under_roots(&root, &extra, std::path::Path::new(path)).map_err(|e| {
        let mut refused = crate::req_path::refused_with(
            "`path` must be relative to the project root or an absolute path under an allowed root \
             (project root, system temp dir, `output_roots` or `replay_roots` in \
             .regia/agent.toml, or a root the host added), without `..`",
            e.code(),
            "use a path relative to the project, or list the replay store in `replay_roots` \
             (.regia/agent.toml) or start the host with `--replay-root DIR`; see \
             book/agents/safety.md (Request paths)",
        );
        crate::req_path::name_allowed_roots(&mut refused, REPLAY_ROOT_KINDS);
        refused
    })
}

/// `replay.load`: reads a `.do27replay`, rewinds to tick 0 (paused) and reports the header,
/// record counts and the verification audit.
pub fn replay_load(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` (the .do27replay file) is required"))?;
    // Confined (ADR-1769): an unconfined path read any host file of up to 512 MiB on the world
    // thread and echoed the size of larger ones. A replay store outside the project is listed in
    // `replay_roots` or [`ReplayRoots`] (ADR-1965).
    let path_buf = confine_replay_path(world, path)?;
    let path_ref = path_buf.as_path();
    // A version 2 full is read with the player's own codec when it knows the layout.
    let file = match world.get_resource::<ReplaySim>() {
        Some(sim) => ReplayFile::load_with(path_ref, sim.codec()),
        None => ReplayFile::load(path_ref),
    }
    .map_err(|e| replay_error(&e))?;
    if world.get_resource::<ReplaySim>().is_none() {
        world.insert_resource(ReplaySim::new());
    }
    let mut sim = world.resource_mut::<ReplaySim>();
    sim.load(file);
    let report = sim.verify();
    let Some(file) = sim.file() else {
        return Err(RpcError::internal("replay vanished after load"));
    };
    Ok(json!({
        "path": path,
        "bytes": file.byte_len,
        "header": header_json(file),
        "end_tick": file.end_tick(),
        "duration_secs": file.duration_secs(),
        "counts": {
            "inputs": file.inputs.len(),
            "fulls": file.fulls.len(),
            "navs": file.navs.len(),
            "salvage": file.salvage.len(),
            "command_records": file.commands.len(),
            "moments": file.moments.len(),
            "moment_ticks": file.moment_batches,
        },
        "abort": file.abort.as_ref().map(|a| json!({ "tick": a.tick, "reason": a.reason })),
        "verify": report.map(|r| json!({
            "ok": r.is_ok(),
            "fulls": r.fulls,
            "hash_checked": r.hash_checked,
            "hash_unchecked": r.hash_unchecked,
            "mismatches": r.mismatches,
            "defects": r.defects,
            "moment_ticks": r.moment_ticks,
            "aborted": r.aborted,
        })),
        "position": position_json(&sim),
    }))
}

/// `replay.seek`: moves to a tick (or second) without firing events; returns the snapshot there.
pub fn replay_seek(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tick = opt_u32(params, "tick")?;
    let seconds = crate::params::opt_f64(params, "seconds")?;
    if tick.is_none() && seconds.is_none() {
        return Err(RpcError::invalid_params("`tick` or `seconds` is required"));
    }
    if seconds.is_some_and(|s| !s.is_finite() || s < 0.0) {
        return Err(RpcError::invalid_params(
            "`seconds` must be a finite, non-negative number",
        ));
    }
    with_loaded(world, |sim| {
        let rate = sim.file().map_or(0, |f| f.header.tick_rate);
        let target = tick.unwrap_or_else(|| {
            (seconds.unwrap_or(0.0) * f64::from(rate))
                .round()
                .min(f64::from(u32::MAX)) as u32
        });
        sim.seek(target);
        let snapshot = sim.snapshot();
        Ok(json!({ "position": position_json(sim), "snapshot": snapshot_json(&snapshot) }))
    })
}

/// `replay.step`: advances `ticks` (default 1) and returns the events crossed.
pub fn replay_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let requested = opt_u32(params, "ticks")?.unwrap_or(1);
    let ticks = requested.min(STEP_TICKS_MAX);
    let presented = match params.get("presented") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`presented` must be a boolean"))?,
    };
    with_loaded(world, |sim| {
        let from = sim.tick();
        let advanced = sim.step(ticks);
        let crossed = sim.last_step_events();
        let events: Vec<Value> = crossed.iter().take(STEP_EVENTS_MAX).map(event_json).collect();
        let events_total = crossed.len();
        // The exact pose, or with `presented` the one a bridge shows (blended between ticks).
        let snapshot = if presented {
            sim.presented_snapshot()
        } else {
            sim.snapshot()
        };
        Ok(json!({
            "from": from,
            "advanced": advanced,
            "clamped": requested > ticks,
            "position": position_json(sim),
            "events": events,
            "events_total": events_total,
            "events_truncated": events_total > events.len(),
            "snapshot": snapshot_json(&snapshot),
        }))
    })
}

/// When `replay.play` starts the clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Start {
    /// The next frame.
    Now,
    /// The next render-out's frame 0.
    RenderOut,
}

/// A `replay.play { start: "render_out" }` waiting for a render-out's frame 0.
#[derive(Resource, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ArmedReplay {
    /// The tick the replay starts from (already clamped to the end).
    pub(crate) from_tick: u32,
}

/// `replay.play`: plays the loaded replay on the simulation clock from `from_tick` (default: where
/// it is) at `speed` (composing with a render-out `time_map`), blending between ticks (`interpolate`, default on, across fulls at most
/// `max_gap_ticks` apart, default 1). With `start: "render_out"` it waits at the tick for the next
/// render-out's frame 0.
pub fn replay_play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let from_tick = opt_u32(params, "from_tick")?;
    let speed = match params.get("speed") {
        None | Some(Value::Null) => 1.0,
        Some(v) => v
            .as_f64()
            .filter(|s| (MIN_SPEED..=MAX_SPEED).contains(s))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`speed` must be a number in {MIN_SPEED}..={MAX_SPEED} (1 = real time, 0.25 = quarter speed)"
                ))
            })?,
    };
    let interpolate = match params.get("interpolate") {
        None | Some(Value::Null) => true,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`interpolate` must be a boolean"))?,
    };
    let max_gap = opt_u32(params, "max_gap_ticks")?.unwrap_or(1);
    if !(1..=MAX_BLEND_GAP_TICKS).contains(&max_gap) {
        return Err(RpcError::invalid_params(format!(
            "`max_gap_ticks` must be an integer in 1..={MAX_BLEND_GAP_TICKS}"
        )));
    }
    let start = match params.get("start") {
        None | Some(Value::Null) => Start::Now,
        Some(v) => match v.as_str() {
            Some("now") => Start::Now,
            Some("render_out") => Start::RenderOut,
            _ => {
                return Err(RpcError::invalid_params(
                    "`start` must be \"now\" (default) or \"render_out\"",
                ));
            }
        },
    };
    let speed_permille = (speed * 1000.0).round() as u32;
    let bridged = world.contains_resource::<Messages<SimEvent<ReplayEvent>>>();
    let (mut out, tick) = with_loaded(world, |sim| {
        if let Some(tick) = from_tick {
            sim.seek(tick);
        }
        sim.set_speed_permille(speed_permille);
        sim.set_interpolation(if interpolate { max_gap } else { 0 });
        sim.set_playing(start == Start::Now);
        let snapshot = sim.snapshot();
        let out = json!({
            "position": position_json(sim),
            "snapshot": snapshot_json(&snapshot),
            "interpolate": interpolate,
            "max_gap_ticks": max_gap,
            "start": if start == Start::Now { "now" } else { "render_out" },
            "driver": if bridged { "sim_bridge" } else { "agent" },
        });
        Ok((out, sim.tick()))
    })?;
    match start {
        Start::Now => {
            world.remove_resource::<ArmedReplay>();
            out["armed_for"] = Value::Null;
        }
        Start::RenderOut => {
            world.insert_resource(ArmedReplay { from_tick: tick });
            out["armed_for"] = json!(crate::render_out::armed_replay_target(world));
        }
    }
    Ok(out)
}

/// `replay.pause`: stops frame-clock playback where it is and drops a `start: "render_out"` arm.
pub fn replay_pause(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let disarmed = world.remove_resource::<ArmedReplay>().is_some();
    with_loaded(world, |sim| {
        sim.set_playing(false);
        Ok(json!({ "position": position_json(sim), "disarmed": disarmed }))
    })
}

/// Called by a render-out as it enters frame 0: starts an armed replay at its tick, holding
/// `hold` (the simulation step into frame 0) so frame 0 shows that tick. Returns the tick, or `None`
/// when nothing was armed or no replay is loaded (the arm is used up either way).
pub(crate) fn start_armed_replay(world: &mut World, hold: Duration) -> Option<u32> {
    let armed = world.remove_resource::<ArmedReplay>()?;
    let mut sim = world.get_resource_mut::<ReplaySim>()?;
    sim.file()?;
    let tick = sim.seek(armed.from_tick);
    sim.set_playing(true);
    sim.hold_clock(hold);
    Some(tick)
}

/// `PreUpdate`: plays a [`ReplaySim`] that is only a resource (no `SimBridgePlugin<ReplaySim>`,
/// whose own system plays it) on the simulation clock (`Time::sim_delta`), so `replay.play`
/// advances headless players too.
pub(crate) fn drive_unbridged_replay(
    time: Option<Res<Time>>,
    sim: Option<ResMut<ReplaySim>>,
    bridged: Option<Res<Messages<SimEvent<ReplayEvent>>>>,
) {
    let (Some(time), Some(mut sim)) = (time, sim) else {
        return;
    };
    if bridged.is_some() || !sim.playing() {
        return;
    }
    sim.advance_clock(time.sim_delta());
}

fn query_from(params: &Value) -> Result<MomentQuery, RpcError> {
    let kind = match params.get("kind") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let text = v.as_str().ok_or_else(|| {
                RpcError::invalid_params("`kind` must be \"hit\", \"down\", \"kill\" or a wire number")
            })?;
            Some(MomentKind::parse(text).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown moment kind {text:?}; use hit, down, kill or a wire number"
                ))
            })?)
        }
    };
    let weapon = match params.get("weapon") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| RpcError::invalid_params("`weapon` must be a string"))?
                .to_owned(),
        ),
    };
    Ok(MomentQuery {
        kind,
        from_tick: opt_u64(params, "from_tick")?,
        to_tick: opt_u64(params, "to_tick")?,
        entity: opt_u32(params, "entity")?,
        attacker: opt_u32(params, "attacker")?,
        victim: opt_u32(params, "victim")?,
        fight: opt_u32(params, "fight")?,
        round: opt_u32(params, "round")?,
        weapon,
        min_importance: opt_u32(params, "min_importance")?.map_or(0, |v| v.min(255) as u8),
    })
}

/// `moments.list`: filtered, paged Moments in emission order.
pub fn moments_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = query_from(params)?;
    let limit = opt_u64(params, "limit")?.map_or(LIST_DEFAULT, |v| (v as usize).min(LIST_MAX));
    let offset = opt_u64(params, "offset")?.map_or(0, |v| v as usize);
    with_loaded(world, |sim| {
        let file = sim.file().ok_or_else(not_loaded)?;
        // One pass, no list of every hit: count the matches, serialize only the requested page.
        let matches = || file.moments.iter().enumerate().filter(|(_, m)| query.matches(m));
        let matched = matches().count();
        let page: Vec<Value> = matches()
            .skip(offset)
            .take(limit)
            .map(|(i, m)| moment_json(Some(i), m))
            .collect();
        Ok(json!({
            "total": file.moments.len(),
            "matched": matched,
            "offset": offset,
            "returned": page.len(),
            "moments": page,
        }))
    })
}

/// `moments.get`: one Moment by index, with its fight and `seek_tick` (the event tick; seek strictly before it).
pub fn moments_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let index =
        opt_u64(params, "index")?.ok_or_else(|| RpcError::invalid_params("`index` is required"))? as usize;
    with_loaded(world, |sim| {
        let file = sim.file().ok_or_else(not_loaded)?;
        let Some(moment) = file.moments.get(index) else {
            return Err(RpcError::not_found(
                format!("no moment {index} (the replay has {})", file.moments.len()),
                "moments.list shows the valid indexes",
            ));
        };
        let fight: Vec<usize> = file
            .moments
            .iter()
            .enumerate()
            .filter(|(_, m)| m.span_fight == moment.span_fight && m.span_round == moment.span_round)
            .map(|(i, _)| i)
            .collect();
        let mut out = moment_json(Some(index), moment);
        out["fight_moments"] = json!(fight);
        out["seek_tick"] = json!(moment.drained_tick);
        Ok(out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_framework::replay_sim::{
        FullCodec, Participant, PilotInput, PlayerRow, ReplayEntity, ReplayHeader, SyntheticReplay,
    };

    fn moment(tick: u64, kind: MomentKind, attacker: u32, victim: u32, fight: u32) -> Moment {
        Moment {
            drained_tick: tick as u32,
            tick,
            kind,
            span_match: 1,
            span_round: 1,
            span_fight: fight,
            participants: [
                Participant {
                    role: 1,
                    entity_id: attacker,
                },
                Participant {
                    role: 2,
                    entity_id: victim,
                },
            ],
            position: Vec3::new(4.0, 1.0, -2.0),
            damage: 25,
            weapon: "rail".into(),
            attacker,
            victim,
            importance: 70,
        }
    }

    fn replay_bytes() -> Vec<u8> {
        let header = ReplayHeader {
            version: 3,
            protocol: "t/1".into(),
            tick_rate: 60,
            lobby_preset: "alpha".into(),
            map: "ironreach".into(),
            mode: "tdm".into(),
            teams: 2,
            players: vec![PlayerRow {
                peer_id: 1,
                account_id: String::new(),
                callsign: "Nova".into(),
                role: 1,
                team: 0,
            }],
            ..ReplayHeader::default()
        };
        let mut w = SyntheticReplay::new(&header);
        for tick in 0..=40u32 {
            w = w.inputs(tick, &[(1, PilotInput::default())]);
            if tick == 0 {
                w = w.full(&[1, 0, 5], 77);
            }
            if tick == 10 {
                w = w.moments(10, &[moment(10, MomentKind::Hit, 1, 2, 1)]);
            }
            if tick == 30 {
                w = w.moments(
                    30,
                    &[
                        moment(30, MomentKind::Kill, 1, 2, 1),
                        moment(30, MomentKind::Hit, 3, 1, 1),
                    ],
                );
            }
        }
        w.finish()
    }

    /// A temp path unique per process and per call, so parallel tests (and the lavapipe and GPU
    /// gate passes running side by side) never share a file.
    fn unique_temp(stem: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("regia-agent-{stem}-{}-{n}", std::process::id()))
    }

    /// Writes the fixture replay atomically (write, then rename) to a fresh path.
    fn write_replay() -> std::path::PathBuf {
        let path = unique_temp("replay").with_extension("do27replay");
        let staging = path.with_extension("tmp");
        std::fs::write(&staging, replay_bytes()).expect("write");
        std::fs::rename(&staging, &path).expect("rename");
        path
    }

    struct Codec;
    impl FullCodec for Codec {
        fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
            Ok(snapshot
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| ReplayEntity {
                    id: u64::from(c[0]),
                    kind: "warhull".into(),
                    team: u32::from(c[1]),
                    translation: Vec3::new(f32::from(c[2]), 0.0, 0.0),
                    rotation: regia_core::math::Quat::IDENTITY,
                })
                .collect())
        }
        fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> {
            Some(77)
        }
    }

    fn loaded_world() -> World {
        let mut world = World::new();
        world.insert_resource(ReplaySim::with_codec(Codec));
        let path = write_replay();
        let out = replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("replay.load");
        let _ = std::fs::remove_file(&path);
        assert_eq!(out["verify"]["ok"], true);
        world
    }

    #[test]
    fn load_reports_header_counts_and_verification() {
        let mut world = World::new();
        let path = write_replay();
        let out = replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("replay.load");
        assert_eq!(out["header"]["map"], "ironreach");
        assert_eq!(out["header"]["players"][0]["role"], "player_host");
        assert_eq!(out["counts"]["fulls"], 1);
        assert_eq!(out["counts"]["moments"], 3);
        assert_eq!(out["end_tick"], 40);
        // No hashing codec on a headless default player: skipped, never "passed".
        assert_eq!(out["verify"]["hash_checked"], 0);
        assert_eq!(out["verify"]["hash_unchecked"], 1);
        assert_eq!(out["position"]["tick"], 0);
        // With a matching codec the same file checks out; a wrong stored hash would not.
        let out =
            replay_load(&mut loaded_world(), &json!({ "path": path.to_string_lossy() })).expect("again");
        assert_eq!(out["verify"]["hash_checked"], 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_errors_are_structured() {
        let mut world = World::new();
        let err = replay_load(&mut world, &json!({})).expect_err("no path");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let missing = unique_temp("replay-missing");
        let err =
            replay_load(&mut world, &json!({ "path": missing.to_string_lossy() })).expect_err("missing");
        assert!(err.message.contains("[io]"), "{}", err.message);
        // Outside every allowed root: refused before any read (ADR-1769).
        let err =
            replay_load(&mut world, &json!({ "path": "/nonexistent/x.do27replay" })).expect_err("outside");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        assert!(!err.message.contains("[io]"), "{}", err.message);
        let bad = unique_temp("replay-bad");
        std::fs::write(&bad, b"NOPE and then some").expect("write");
        let err = replay_load(&mut world, &json!({ "path": bad.to_string_lossy() })).expect_err("bad magic");
        assert!(err.message.contains("[bad_magic]"), "{}", err.message);
        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn calls_before_load_are_not_found() {
        let mut world = World::new();
        for err in [
            replay_seek(&mut world, &json!({ "tick": 1 })).expect_err("seek"),
            replay_step(&mut world, &json!({})).expect_err("step"),
            moments_list(&mut world, &json!({})).expect_err("list"),
            moments_get(&mut world, &json!({ "index": 0 })).expect_err("get"),
        ] {
            assert_eq!(err.code, crate::protocol::NOT_FOUND);
        }
    }

    #[test]
    fn step_clamps_ticks_and_caps_serialized_events() {
        let mut world = loaded_world();
        let out = replay_step(&mut world, &json!({ "ticks": u32::MAX })).expect("step");
        // The fixture is 40 ticks long: the clamp only shows on longer replays, the totals always.
        assert_eq!(out["advanced"], 40);
        assert_eq!(out["clamped"], true);
        assert_eq!(out["events_total"], 3);
        assert_eq!(out["events_truncated"], false);
        assert_eq!(out["events"].as_array().map(Vec::len), Some(3));
        let out = replay_step(&mut world, &json!({ "ticks": 1 })).expect("past the end");
        assert_eq!(out["clamped"], false);
        assert_eq!(out["events_total"], 0);
    }

    #[test]
    fn moments_list_pages_without_changing_counts() {
        let mut world = loaded_world();
        let out = moments_list(&mut world, &json!({ "limit": 1, "offset": 1 })).expect("page");
        assert_eq!(out["total"], 3);
        assert_eq!(out["matched"], 3);
        assert_eq!(out["returned"], 1);
        assert_eq!(out["moments"][0]["index"], 1);
        let out = moments_list(&mut world, &json!({ "kind": "kill", "offset": 5 })).expect("past the end");
        assert_eq!(out["matched"], 1);
        assert_eq!(out["returned"], 0);
    }

    #[test]
    fn seek_returns_the_snapshot_and_is_repeatable() {
        let mut world = loaded_world();
        let a = replay_seek(&mut world, &json!({ "tick": 25 })).expect("replay.seek");
        assert_eq!(a["position"]["tick"], 25);
        assert_eq!(a["snapshot"]["entity_count"], 1);
        assert_eq!(a["snapshot"]["entities"][0]["position"], json!([5.0, 0.0, 0.0]));
        replay_seek(&mut world, &json!({ "tick": 3 })).expect("back");
        let b = replay_seek(&mut world, &json!({ "tick": 25 })).expect("again");
        assert_eq!(a, b, "seeking is a pure function of the tick");
        let s = replay_seek(&mut world, &json!({ "seconds": 0.5 })).expect("seconds");
        assert_eq!(s["position"]["tick"], 30);
        let clamped = replay_seek(&mut world, &json!({ "tick": 99999 })).expect("clamp");
        assert_eq!(clamped["position"]["tick"], 40);
        assert_eq!(clamped["position"]["at_end"], true);
        assert!(replay_seek(&mut world, &json!({})).is_err());
        assert!(replay_seek(&mut world, &json!({ "tick": -1 })).is_err());
        assert!(replay_seek(&mut world, &json!({ "seconds": -1.0 })).is_err());
    }

    #[test]
    fn step_reports_the_events_it_crossed() {
        let mut world = loaded_world();
        let quiet = replay_step(&mut world, &json!({ "ticks": 9 })).expect("replay.step");
        assert_eq!(quiet["advanced"], 9);
        assert_eq!(quiet["events"], json!([]));
        let hit = replay_step(&mut world, &json!({})).expect("one tick");
        assert_eq!(hit["position"]["tick"], 10);
        assert_eq!(hit["events"][0]["type"], "moment");
        assert_eq!(hit["events"][0]["moment"]["kind"], "hit");
        let rest = replay_step(&mut world, &json!({ "ticks": 1000 })).expect("to the end");
        assert_eq!(rest["events"].as_array().map(Vec::len), Some(2));
        assert_eq!(rest["position"]["at_end"], true);
        let again = replay_step(&mut world, &json!({ "ticks": 5 })).expect("past the end");
        assert_eq!(again["advanced"], 0);
        assert_eq!(again["events"], json!([]));
    }

    #[test]
    fn moments_list_filters_and_pages() {
        let mut world = loaded_world();
        let all = moments_list(&mut world, &json!({})).expect("moments.list");
        assert_eq!(all["total"], 3);
        assert_eq!(all["returned"], 3);
        assert_eq!(all["moments"][0]["index"], 0);
        assert_eq!(all["moments"][0]["position"], json!([4.0, 1.0, -2.0]));
        let kills = moments_list(&mut world, &json!({ "kind": "kill" })).expect("kills");
        assert_eq!(kills["matched"], 1);
        assert_eq!(kills["moments"][0]["index"], 1);
        let by_entity = moments_list(&mut world, &json!({ "entity": 3 })).expect("entity");
        assert_eq!(by_entity["matched"], 1);
        let window = moments_list(
            &mut world,
            &json!({ "from_tick": 11, "to_tick": 30, "limit": 1, "offset": 1 }),
        )
        .expect("window");
        assert_eq!(window["matched"], 2);
        assert_eq!(window["returned"], 1);
        assert_eq!(window["moments"][0]["index"], 2);
        assert!(moments_list(&mut world, &json!({ "kind": "explosion" })).is_err());
        assert!(moments_list(&mut world, &json!({ "limit": "ten" })).is_err());
    }

    #[test]
    fn moments_get_returns_one_with_its_fight() {
        let mut world = loaded_world();
        let m = moments_get(&mut world, &json!({ "index": 1 })).expect("moments.get");
        assert_eq!(m["kind"], "kill");
        assert_eq!(m["weapon"], "rail");
        assert_eq!(m["seek_tick"], 30);
        assert_eq!(m["fight_moments"], json!([0, 1, 2]));
        // seek_tick is the event tick. A seek onto it, then one step, does not deliver the Moment.
        let tick = m["seek_tick"].as_u64().expect("seek_tick");
        replay_seek(&mut world, &json!({ "tick": tick })).expect("seek onto the event tick");
        let skipped = replay_step(&mut world, &json!({ "ticks": 1 })).expect("step");
        assert_eq!(
            skipped["events"],
            json!([]),
            "the step after seeking to seek_tick does not play the Moment"
        );
        replay_seek(&mut world, &json!({ "tick": tick - 1 })).expect("seek strictly before");
        let played = replay_step(&mut world, &json!({ "ticks": 1 })).expect("step onto the tick");
        assert_eq!(played["events"].as_array().map(Vec::len), Some(2));
        assert_eq!(played["events"][0]["type"], "moment");
        assert_eq!(played["events"][0]["tick"], 30);
        assert_eq!(played["events"][0]["moment"]["kind"], "kill");
        let err = moments_get(&mut world, &json!({ "index": 9 })).expect_err("out of range");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        assert!(moments_get(&mut world, &json!({})).is_err());
    }

    // ---- Error-path and semantics table tests (agent-tests lane) ------------------------------

    #[test]
    fn load_rejects_non_string_and_null_paths() {
        let mut world = World::new();
        for params in [
            json!({ "path": 5 }),
            json!({ "path": null }),
            json!({ "path": ["a"] }),
            json!(null),
        ] {
            let err = replay_load(&mut world, &params).expect_err("bad path");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
        // A failed load never creates the player.
        assert!(world.get_resource::<ReplaySim>().is_none());
    }

    #[test]
    fn load_of_a_directory_and_of_garbage_are_structured_errors() {
        let mut world = World::new();
        let dir = std::env::temp_dir();
        let err = replay_load(&mut world, &json!({ "path": dir.to_string_lossy() })).expect_err("directory");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        assert!(err.message.contains("[io]"), "{}", err.message);
        let tiny = std::env::temp_dir().join(format!("regia-agent-replay-tiny-{}", std::process::id()));
        std::fs::write(&tiny, b"DO27").expect("write");
        let err = replay_load(&mut world, &json!({ "path": tiny.to_string_lossy() })).expect_err("truncated");
        assert!(err.message.contains("[truncated]"), "{}", err.message);
        let _ = std::fs::remove_file(&tiny);
    }

    #[test]
    fn failed_load_keeps_the_previous_replay() {
        let mut world = loaded_world();
        replay_seek(&mut world, &json!({ "tick": 12 })).expect("seek");
        replay_load(&mut world, &json!({ "path": "/nonexistent/x.do27replay" })).expect_err("missing");
        let still = replay_step(&mut world, &json!({ "ticks": 0 })).expect("still loaded");
        assert_eq!(
            still["position"]["tick"], 12,
            "a failed load must not rewind or unload"
        );
    }

    #[test]
    fn reloading_rewinds_to_tick_zero_and_is_idempotent() {
        let mut world = loaded_world();
        replay_step(&mut world, &json!({ "ticks": 20 })).expect("step");
        let path = write_replay();
        let a = replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("reload");
        assert_eq!(a["position"]["tick"], 0);
        let b = replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("reload again");
        assert_eq!(a, b, "loading the same file twice answers identically");
    }

    #[test]
    fn seek_param_validation_table() {
        let mut world = loaded_world();
        for (params, why) in [
            (json!({}), "neither tick nor seconds"),
            (json!(null), "null params"),
            (json!({ "tick": "5" }), "string tick"),
            (json!({ "tick": 1.5 }), "fractional tick"),
            (json!({ "tick": -1 }), "negative tick"),
            (json!({ "tick": u64::from(u32::MAX) + 1 }), "tick above u32"),
            (json!({ "seconds": -0.5 }), "negative seconds"),
        ] {
            let err = replay_seek(&mut world, &params).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
        // `tick` wins over `seconds` when both are given.
        let both = replay_seek(&mut world, &json!({ "tick": 7, "seconds": 0.5 })).expect("both");
        assert_eq!(both["position"]["tick"], 7);
        // Seeking to tick 0 is allowed and is the start.
        let zero = replay_seek(&mut world, &json!({ "tick": 0 })).expect("zero");
        assert_eq!(zero["position"]["at_end"], false);
        // Seeking to the exact end tick.
        let end = replay_seek(&mut world, &json!({ "tick": 40 })).expect("end");
        assert_eq!(end["position"]["at_end"], true);
    }

    #[test]
    fn seek_fires_no_events_and_does_not_disturb_step() {
        let mut world = loaded_world();
        replay_seek(&mut world, &json!({ "tick": 35 })).expect("seek past the kill");
        // Stepping from the seek point only reports what is crossed after it.
        let rest = replay_step(&mut world, &json!({ "ticks": 100 })).expect("rest");
        assert_eq!(rest["events"], json!([]));
        // Rewind and walk through: both moment batches arrive exactly once.
        replay_seek(&mut world, &json!({ "tick": 0 })).expect("rewind");
        let walk = replay_step(&mut world, &json!({ "ticks": 100 })).expect("walk");
        assert_eq!(walk["events"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn step_param_validation_and_zero_step() {
        let mut world = loaded_world();
        for params in [
            json!({ "ticks": -1 }),
            json!({ "ticks": "x" }),
            json!({ "ticks": 1.5 }),
            json!({ "ticks": u64::MAX }),
        ] {
            let err = replay_step(&mut world, &params).expect_err("bad ticks");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
        }
        let zero = replay_step(&mut world, &json!({ "ticks": 0 })).expect("zero");
        assert_eq!(zero["advanced"], 0);
        assert_eq!(zero["position"]["tick"], 0);
        // null params use the default of one tick.
        let one = replay_step(&mut world, &json!({ "ticks": null })).expect("null ticks");
        assert_eq!(one["advanced"], 1);
    }

    #[test]
    fn step_and_seek_agree_on_the_snapshot() {
        let mut stepped = loaded_world();
        let s = replay_step(&mut stepped, &json!({ "ticks": 25 })).expect("step");
        let mut sought = loaded_world();
        let k = replay_seek(&mut sought, &json!({ "tick": 25 })).expect("seek");
        assert_eq!(
            s["snapshot"], k["snapshot"],
            "stepping and seeking reach the same state"
        );
        assert_eq!(s["position"], k["position"]);
    }

    #[test]
    fn whole_replay_is_deterministic_across_independent_players() {
        let walk = || {
            let mut world = loaded_world();
            let mut out = Vec::new();
            for _ in 0..8 {
                out.push(replay_step(&mut world, &json!({ "ticks": 6 })).expect("step"));
            }
            out
        };
        assert_eq!(walk(), walk());
    }

    #[test]
    fn moments_list_param_validation_table() {
        let mut world = loaded_world();
        for (params, why) in [
            (json!({ "kind": 5 }), "numeric kind must be a string"),
            (json!({ "kind": "" }), "empty kind"),
            (json!({ "kind": "nope" }), "unknown kind"),
            (json!({ "weapon": 1 }), "numeric weapon"),
            (json!({ "limit": -1 }), "negative limit"),
            (json!({ "offset": "a" }), "string offset"),
            (json!({ "from_tick": -3 }), "negative from_tick"),
            (json!({ "entity": u64::from(u32::MAX) + 1 }), "entity above u32"),
            (json!({ "fight": 1.5 }), "fractional fight"),
        ] {
            let err = moments_list(&mut world, &params).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
    }

    #[test]
    fn moments_list_filters_by_every_field() {
        let mut world = loaded_world();
        let mut count = |params: Value| moments_list(&mut world, &params).expect("list")["matched"].clone();
        assert_eq!(count(json!({ "attacker": 3 })), 1);
        assert_eq!(count(json!({ "victim": 2 })), 2);
        assert_eq!(count(json!({ "fight": 1, "round": 1 })), 3);
        assert_eq!(count(json!({ "fight": 2 })), 0);
        assert_eq!(count(json!({ "weapon": "rail" })), 3);
        assert_eq!(count(json!({ "weapon": "plasma" })), 0);
        assert_eq!(count(json!({ "min_importance": 71 })), 0);
        assert_eq!(count(json!({ "min_importance": 70 })), 3);
        assert_eq!(
            count(json!({ "min_importance": 100000 })),
            0,
            "saturates at 255 without erroring"
        );
        assert_eq!(count(json!({ "kind": "hit" })), 2);
        assert_eq!(count(json!({ "kind": "down" })), 0);
        assert_eq!(count(json!({ "kind": null })), 3);
        assert_eq!(count(json!({ "from_tick": 31 })), 0);
        assert_eq!(count(json!({ "to_tick": 9 })), 0);
    }

    #[test]
    fn moments_list_paging_edges() {
        let mut world = loaded_world();
        let none = moments_list(&mut world, &json!({ "limit": 0 })).expect("limit 0");
        assert_eq!(none["returned"], 0);
        assert_eq!(none["matched"], 3);
        let past = moments_list(&mut world, &json!({ "offset": 99 })).expect("offset past");
        assert_eq!(past["returned"], 0);
        assert_eq!(past["moments"], json!([]));
        let huge = moments_list(&mut world, &json!({ "limit": u64::MAX, "offset": u64::MAX })).expect("huge");
        assert_eq!(huge["returned"], 0);
        let big = moments_list(&mut world, &json!({ "limit": 1_000_000 })).expect("clamped limit");
        assert_eq!(big["returned"], 3);
        // Two pages cover the list once, in emission order.
        let p1 = moments_list(&mut world, &json!({ "limit": 2 })).expect("p1");
        let p2 = moments_list(&mut world, &json!({ "limit": 2, "offset": 2 })).expect("p2");
        let idx: Vec<_> = p1["moments"]
            .as_array()
            .into_iter()
            .chain(p2["moments"].as_array())
            .flatten()
            .map(|m| m["index"].clone())
            .collect();
        assert_eq!(idx, vec![json!(0), json!(1), json!(2)]);
    }

    #[test]
    fn moments_get_error_table_and_idempotence() {
        let mut world = loaded_world();
        for (params, why) in [
            (json!({}), "missing index"),
            (json!({ "index": null }), "null index"),
            (json!({ "index": -1 }), "negative index"),
            (json!({ "index": "0" }), "string index"),
            (json!({ "index": 0.5 }), "fractional index"),
        ] {
            let err = moments_get(&mut world, &params).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
        for index in [3u64, 1000, u64::from(u32::MAX), u64::MAX] {
            let err = moments_get(&mut world, &json!({ "index": index })).expect_err("out of range");
            assert_eq!(err.code, crate::protocol::NOT_FOUND, "index {index}");
        }
        let a = moments_get(&mut world, &json!({ "index": 2 })).expect("get");
        let b = moments_get(&mut world, &json!({ "index": 2 })).expect("again");
        assert_eq!(a, b);
        assert_eq!(a["index"], 2);
        assert_eq!(a["seek_tick"], 30);
    }

    #[test]
    fn moments_do_not_move_the_playhead() {
        let mut world = loaded_world();
        replay_seek(&mut world, &json!({ "tick": 17 })).expect("seek");
        moments_list(&mut world, &json!({})).expect("list");
        moments_get(&mut world, &json!({ "index": 0 })).expect("get");
        let now = replay_step(&mut world, &json!({ "ticks": 0 })).expect("probe");
        assert_eq!(now["position"]["tick"], 17);
    }

    #[test]
    fn snapshot_summary_truncates_large_entity_lists() {
        // 150 entities of 3 bytes each in the full at tick 0.
        let header = ReplayHeader {
            version: 3,
            tick_rate: 60,
            ..ReplayHeader::default()
        };
        let snap: Vec<u8> = (0..150u32).flat_map(|i| [(i % 250) as u8 + 1, 0, 1]).collect();
        let bytes = SyntheticReplay::new(&header)
            .inputs(0, &[(1, PilotInput::default())])
            .full(&snap, 77)
            .finish();
        let path = std::env::temp_dir().join(format!(
            "regia-agent-replay-big-{}.do27replay",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("write");
        let mut world = World::new();
        world.insert_resource(ReplaySim::with_codec(Codec));
        replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("load");
        let out = replay_seek(&mut world, &json!({ "tick": 0 })).expect("seek");
        assert_eq!(out["snapshot"]["entity_count"], 150);
        assert_eq!(
            out["snapshot"]["entities"].as_array().map(Vec::len),
            Some(SNAPSHOT_ENTITY_CAP)
        );
        assert_eq!(out["snapshot"]["truncated"], true);
        let _ = std::fs::remove_file(&path);
    }

    /// A canonical path as a host or an agent spells a folder outside the project: on Windows
    /// `canonicalize` returns the verbatim form `\\?\C:\...`, which the request-path rule takes for a
    /// remote form and counts for nothing in `output_roots`/`replay_roots` (ADR-3206), so the
    /// `C:\...` spelling is used there (ADR-7566). Elsewhere the path is returned unchanged.
    fn drive_spelling(path: std::path::PathBuf) -> std::path::PathBuf {
        #[cfg(windows)]
        if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return std::path::PathBuf::from(rest);
        }
        path
    }

    /// The frozen `.do27replay` fixtures (ADR-1378) through the protocol: the version 3 and 2 files
    /// load with every count, and the future-version and unknown-tag files answer their documented
    /// codes without replacing the replay loaded before them.
    /// `replay.load`'s roots (ADR-1965): every allow and refusal shape. The "outside" directory is
    /// the fixture folder of another crate: not the project or the temp dir. The working
    /// directory no longer grants access (ADR-7871).
    #[test]
    fn replay_load_reads_only_under_its_roots() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../regia_framework/tests/fixtures/replay")
            .canonicalize()
            .expect("fixture dir");
        let temp = std::env::temp_dir().canonicalize().expect("temp dir");
        if fixtures.starts_with(&temp) {
            eprintln!("skip: the source tree lies under the temp dir, nothing is outside");
            return;
        }
        let fixtures = drive_spelling(fixtures);
        let outside = fixtures.join("v3_every_record.do27replay");
        let project = regia_test_utils::unique_temp_dir("replay-roots")
            .canonicalize()
            .expect("project");
        std::fs::create_dir_all(project.join("replays")).expect("mkdir");
        std::fs::copy(&outside, project.join("replays/a.do27replay")).expect("copy");
        let world_with = |config: crate::safety::AgentConfig, host: Option<ReplayRoots>| {
            let mut world = World::new();
            world.insert_resource(crate::AgentSettings {
                project_dir: Some(project.clone()),
            });
            world.insert_resource(crate::safety::AgentPermissions { config, source: None });
            if let Some(roots) = host {
                world.insert_resource(roots);
            }
            world
        };
        let plain = || world_with(crate::safety::AgentConfig::default(), None);
        let load = |world: &mut World, path: &str| replay_load(world, &json!({ "path": path }));
        let refused = |world: &mut World, path: &str, rule: &str| {
            let err = load(world, path).expect_err(path);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{path}");
            let data = err.data.clone().expect("data");
            assert_eq!(data["code"], "F0002", "{path}");
            assert_eq!(data["reason"], "path_outside_project", "{path}");
            assert_eq!(data["path_reason"], rule, "{path}: {data}");
            assert_eq!(data["allowed_roots"], json!(REPLAY_ROOT_KINDS), "{path}");
            // The refusal names root kinds, never a host path.
            let text = data.to_string();
            assert!(!text.contains(&*fixtures.to_string_lossy()), "{text}");
            assert!(!text.contains(&*project.to_string_lossy()), "{text}");
            assert!(err.message.contains("replay_roots"), "{}", err.message);
        };

        // Relative to the project, and absolute inside it: loaded.
        load(&mut plain(), "replays/a.do27replay").expect("relative");
        load(
            &mut plain(),
            &project.join("replays/a.do27replay").to_string_lossy(),
        )
        .expect("absolute inside");
        // `..`, relative or absolute, even when it lands inside: refused.
        refused(&mut plain(), "../a.do27replay", "path_parent");
        // Built as a string with the host separator: a `\\?\` (verbatim) root on Windows takes
        // `/` literally, and `PathBuf::join("..")` on a verbatim path pops the last component, so
        // the `..` never reached the request (ADR-3150).
        let sep = std::path::MAIN_SEPARATOR;
        let parent = format!(
            "{}{sep}..{sep}replays{sep}a.do27replay",
            project.join("replays").display()
        );
        assert!(parent.contains(".."), "{parent}");
        refused(&mut plain(), &parent, "path_parent");
        // Absolute outside every root: refused, until a root lists it.
        let outside = outside.to_string_lossy().into_owned();
        refused(&mut plain(), &outside, "path_escapes");
        let config = crate::safety::AgentConfig {
            replay_roots: vec![fixtures.clone()],
            ..crate::safety::AgentConfig::default()
        };
        load(&mut world_with(config, None), &outside).expect("agent.toml replay_roots");
        let host = ReplayRoots(vec![fixtures.clone()]);
        load(
            &mut world_with(crate::safety::AgentConfig::default(), Some(host)),
            &outside,
        )
        .expect("a host replay root");
        let config = crate::safety::AgentConfig {
            output_roots: vec![fixtures.clone()],
            ..crate::safety::AgentConfig::default()
        };
        load(&mut world_with(config, None), &outside).expect("output_roots still count");
        // A root that does not exist, or another one, does not open the outside.
        let host = ReplayRoots(vec![project.join("missing"), project.join("replays")]);
        refused(
            &mut world_with(crate::safety::AgentConfig::default(), Some(host)),
            &outside,
            "path_escapes",
        );
        // A symlink in the project that leads out is refused; it loads once its target is a root.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&fixtures, project.join("store")).expect("symlink");
            refused(&mut plain(), "store/v3_every_record.do27replay", "path_escapes");
            let host = ReplayRoots(vec![fixtures.clone()]);
            load(
                &mut world_with(crate::safety::AgentConfig::default(), Some(host)),
                "store/v3_every_record.do27replay",
            )
            .expect("the symlink lands under a replay root");
        }
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn frozen_fixtures_load_or_answer_their_codes() {
        // The fixtures live in another crate: their folder is an `output_roots` entry, the way a
        // host lists a replay folder outside the project (ADR-1769).
        let dir = drive_spelling(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../regia_framework/tests/fixtures/replay")
                .canonicalize()
                .expect("fixture dir"),
        );
        let fixture = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let mut world = World::new();
        world.insert_resource(crate::safety::AgentPermissions {
            config: crate::safety::AgentConfig {
                output_roots: vec![dir.clone()],
                ..crate::safety::AgentConfig::default()
            },
            source: None,
        });
        let out = replay_load(
            &mut world,
            &json!({ "path": fixture("v3_every_record.do27replay") }),
        )
        .expect("the v3 fixture loads");
        assert_eq!(out["bytes"], 699);
        assert_eq!(out["header"]["version"], 3);
        assert_eq!(out["header"]["seed"], "81985529216486895");
        assert_eq!(out["header"]["players"][1]["callsign"], "Ödön");
        assert_eq!(out["header"]["peers"][2]["role"], "spectator");
        assert_eq!(
            out["counts"],
            json!({
                "inputs": 3, "fulls": 2, "navs": 1, "salvage": 1, "command_records": 1,
                "moments": 3, "moment_ticks": 2,
            })
        );
        assert_eq!(out["abort"], json!({ "tick": 2, "reason": "host left" }));
        assert_eq!(out["end_tick"], 2);
        // No codec on a headless default player: both fulls are skipped, never passed.
        assert_eq!(out["verify"]["hash_checked"], 0);
        assert_eq!(out["verify"]["hash_unchecked"], 2);

        let v2 = replay_load(&mut world, &json!({ "path": fixture("v2_no_fulls.do27replay") }))
            .expect("the v2 fixture loads");
        assert_eq!(v2["header"]["version"], 2);
        assert_eq!(v2["header"]["players"], json!([]));
        assert_eq!(
            v2["verify"]["ok"], false,
            "a file with no full is reported, not passed"
        );

        for (name, code) in [
            ("v4_future.do27replay", "[truncated]"),
            ("v3_unknown_tag.do27replay", "[unknown_tag]"),
        ] {
            let err = replay_load(&mut world, &json!({ "path": fixture(name) })).expect_err(name);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
            assert!(err.message.contains(code), "{name}: {}", err.message);
        }
        // The refused files left the v2 replay loaded.
        let at = replay_seek(&mut world, &json!({ "tick": 5 })).expect("still loaded");
        assert_eq!(at["position"]["end_tick"], 1);
    }

    // ---- replay.play / replay.pause, the render-out arm, v2 fulls (ADR-1519) ------------------

    /// One full per tick for ticks 0..=20: entity 1 at x = 3 · tick.
    fn every_tick_world() -> World {
        let header = ReplayHeader {
            version: 3,
            tick_rate: 60,
            ..ReplayHeader::default()
        };
        let mut w = SyntheticReplay::new(&header);
        for tick in 0..=20u32 {
            w = w.inputs(tick, &[]).full(&[1, 0, (3 * tick) as u8], 77);
        }
        let path = unique_temp("replay-every").with_extension("do27replay");
        let staging = path.with_extension("tmp");
        std::fs::write(&staging, w.finish()).expect("write");
        std::fs::rename(&staging, &path).expect("rename");
        let mut world = World::new();
        world.insert_resource(ReplaySim::with_codec(Codec));
        replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("replay.load");
        let _ = std::fs::remove_file(&path);
        world
    }

    /// Runs the unbridged driver once with a game delta of `delta`.
    fn drive(world: &mut World, delta: Duration) {
        let mut time = Time::default();
        time.advance(delta);
        world.insert_resource(time);
        world
            .run_system_cached(drive_unbridged_replay)
            .expect("driver runs");
    }

    #[test]
    fn play_validates_its_parameters() {
        let mut world = loaded_world();
        for (params, why) in [
            (json!({ "speed": 0 }), "zero speed"),
            (json!({ "speed": -1 }), "negative speed"),
            (json!({ "speed": 65 }), "speed above 64"),
            (json!({ "speed": "fast" }), "string speed"),
            (json!({ "from_tick": -1 }), "negative tick"),
            (json!({ "from_tick": 1.5 }), "fractional tick"),
            (json!({ "interpolate": "yes" }), "string interpolate"),
            (json!({ "max_gap_ticks": 0 }), "zero gap"),
            (json!({ "max_gap_ticks": 601 }), "gap above 600"),
            (json!({ "start": "later" }), "unknown start"),
            (json!({ "start": 1 }), "numeric start"),
        ] {
            let err = replay_play(&mut world, &params).expect_err(why);
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{why}");
        }
        // Nothing was changed by the refused calls.
        let sim = world.resource::<ReplaySim>();
        assert!(!sim.playing());
        assert_eq!((sim.tick(), sim.speed_permille()), (0, 1000));
        // Before a load it is NOT_FOUND, like the other replay calls.
        let mut empty = World::new();
        for err in [
            replay_play(&mut empty, &json!({})).expect_err("play"),
            replay_pause(&mut empty, &json!({})).expect_err("pause"),
        ] {
            assert_eq!(err.code, crate::protocol::NOT_FOUND);
        }
    }

    #[test]
    fn play_now_advances_on_the_game_clock_without_a_bridge() {
        let mut world = loaded_world();
        let out = replay_play(&mut world, &json!({ "from_tick": 5, "speed": 0.5 })).expect("replay.play");
        assert_eq!(out["position"]["tick"], 5);
        assert_eq!(out["position"]["playing"], true);
        assert_eq!(out["position"]["speed"], 0.5);
        assert_eq!(out["driver"], "agent");
        assert_eq!(out["start"], "now");
        assert_eq!(out["armed_for"], Value::Null);
        // 60 ticks/s at half speed: 30 ticks per game second, the fraction carried over.
        drive(&mut world, Duration::from_millis(250));
        assert_eq!(
            world.resource::<ReplaySim>().tick(),
            5 + 7,
            "0.25 s at half speed is 7.5 ticks"
        );
        drive(&mut world, Duration::from_millis(250));
        assert_eq!(world.resource::<ReplaySim>().tick(), 5 + 15);
        // Four more game seconds would be 120 ticks: clamped to the 40-tick end.
        drive(&mut world, Duration::from_secs(4));
        let sim = world.resource::<ReplaySim>();
        assert_eq!(sim.tick(), 40);
        assert!(!sim.playing(), "playback stops at the end");
    }

    #[test]
    fn a_bridged_player_is_left_to_the_bridge() {
        let mut world = loaded_world();
        world.init_resource::<Messages<regia_framework::sim_bridge::SimEvent<ReplayEvent>>>();
        let out = replay_play(&mut world, &json!({})).expect("replay.play");
        assert_eq!(out["driver"], "sim_bridge");
        drive(&mut world, Duration::from_secs(1));
        assert_eq!(
            world.resource::<ReplaySim>().tick(),
            0,
            "the agent never advances a player the bridge plays"
        );
    }

    #[test]
    fn start_render_out_waits_at_the_tick_and_starts_on_frame_zero() {
        let mut world = loaded_world();
        let out = replay_play(
            &mut world,
            &json!({ "from_tick": 10, "speed": 2, "start": "render_out" }),
        )
        .expect("armed");
        assert_eq!(out["start"], "render_out");
        assert_eq!(out["armed_for"], "next_job", "no render-out is running");
        assert_eq!(out["position"]["playing"], false, "it waits at the tick");
        drive(&mut world, Duration::from_secs(1));
        assert_eq!(
            world.resource::<ReplaySim>().tick(),
            10,
            "nothing plays before frame 0"
        );
        // The render-out enters frame 0: the step into it (here 1/30 s) is held.
        let step = Duration::from_nanos(33_333_333);
        assert_eq!(start_armed_replay(&mut world, step), Some(10));
        assert!(
            world.get_resource::<ArmedReplay>().is_none(),
            "the arm is used once"
        );
        drive(&mut world, step);
        let sim = world.resource::<ReplaySim>();
        assert_eq!(
            (sim.tick(), sim.tick_fraction()),
            (10, 0.0),
            "frame 0 shows the stated tick"
        );
        // From then on: tick = 10 + 60 · 2 · game time.
        drive(&mut world, Duration::from_millis(100));
        assert_eq!(world.resource::<ReplaySim>().tick(), 22);
        assert_eq!(
            start_armed_replay(&mut world, step),
            None,
            "nothing armed any more"
        );
    }

    #[test]
    fn replay_speed_composes_with_the_sim_step() {
        // A render-out `time_map` at 0.25 sets the simulation step to a quarter of its 40 ms frame
        // (`SimStep`); a 2x replay then plays 60 · 2 · 0.01 = 1.2 ticks a frame (ADR-1524).
        let run = |sim_step: Option<Duration>| {
            let mut world = loaded_world();
            let sim = world.remove_resource::<ReplaySim>().expect("player");
            let mut app = App::new();
            app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(40)));
            if let Some(step) = sim_step {
                app.insert_resource(SimStep(step));
            }
            app.insert_resource(sim);
            app.add_systems(PreUpdate, drive_unbridged_replay);
            replay_play(app.world_mut(), &json!({ "from_tick": 0, "speed": 2 })).expect("play");
            for _ in 0..5 {
                app.update();
            }
            let time = app.world().resource::<Time>();
            assert_eq!(
                time.elapsed(),
                Duration::from_millis(200),
                "the presentation clock"
            );
            let sim = app.world().resource::<ReplaySim>();
            (sim.tick(), sim.tick_fraction())
        };
        // 5 frames · 10 ms · 2 · 60 ticks/s = 6 ticks exactly.
        assert_eq!(run(Some(Duration::from_millis(10))), (6, 0.0));
        // Without a map the replay follows the 40 ms frames: 24 ticks.
        assert_eq!(run(None), (24, 0.0));
    }

    #[test]
    fn pause_stops_playback_and_drops_an_arm() {
        let mut world = loaded_world();
        replay_play(&mut world, &json!({ "start": "render_out", "from_tick": 3 })).expect("arm");
        let out = replay_pause(&mut world, &json!({})).expect("pause");
        assert_eq!(out["disarmed"], true);
        assert_eq!(out["position"]["tick"], 3);
        assert_eq!(start_armed_replay(&mut world, Duration::ZERO), None);
        replay_play(&mut world, &json!({})).expect("play");
        drive(&mut world, Duration::from_millis(100));
        let out = replay_pause(&mut world, &json!({})).expect("pause again");
        assert_eq!(out["disarmed"], false);
        assert_eq!(out["position"]["playing"], false);
        assert_eq!(out["position"]["tick"], 9);
        drive(&mut world, Duration::from_secs(1));
        assert_eq!(world.resource::<ReplaySim>().tick(), 9, "paused players stay put");
    }

    #[test]
    fn slow_motion_snapshots_blend_between_ticks() {
        let mut world = every_tick_world();
        // Quarter speed: 1/60 s of game time is a quarter tick.
        replay_play(&mut world, &json!({ "from_tick": 4, "speed": 0.25 })).expect("play");
        drive(&mut world, Duration::from_millis(30));
        let exact = replay_step(&mut world, &json!({ "ticks": 0 })).expect("exact");
        assert_eq!(
            exact["snapshot"]["entities"][0]["position"][0], 12.0,
            "the exact snapshot holds the tick's full: blending is presentation only"
        );
        assert_eq!(exact["snapshot"]["blend"], Value::Null);
        let probe = replay_step(&mut world, &json!({ "ticks": 0, "presented": true })).expect("probe");
        assert_eq!(probe["position"]["tick"], 4);
        let fraction = probe["position"]["tick_fraction"].as_f64().expect("fraction");
        assert!((fraction - 0.45).abs() < 1e-6, "{fraction}");
        let x = probe["snapshot"]["entities"][0]["position"][0]
            .as_f64()
            .expect("x");
        assert!((x - 3.0 * 4.45).abs() < 1e-4, "x {x}");
        assert_eq!(probe["snapshot"]["blend"]["next_full_tick"], 5);
        // interpolate: false holds the pose for the whole tick.
        replay_play(
            &mut world,
            &json!({ "from_tick": 4, "speed": 0.25, "interpolate": false }),
        )
        .expect("play");
        drive(&mut world, Duration::from_millis(30));
        let held = replay_step(&mut world, &json!({ "ticks": 0, "presented": true })).expect("probe");
        assert_eq!(held["snapshot"]["entities"][0]["position"][0], 12.0);
        assert_eq!(held["snapshot"]["blend"], Value::Null);
        let err = replay_step(&mut world, &json!({ "ticks": 0, "presented": "yes" })).expect_err("bad flag");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
    }

    /// A made-up version 2 layout the test codec knows: a count byte, then 3-byte rows.
    struct V2Codec;
    impl FullCodec for V2Codec {
        fn decode(&self, snapshot: &[u8]) -> Result<Vec<ReplayEntity>, String> {
            Codec.decode(snapshot.get(1..).unwrap_or_default())
        }
        fn state_hash(&self, _snapshot: &[u8]) -> Option<u64> {
            None
        }
        fn v2_full_len(&self, body: &[u8]) -> Option<usize> {
            body.first().map(|n| 1 + 3 * usize::from(*n))
        }
    }

    #[test]
    fn a_version_2_full_loads_only_with_a_codec_that_knows_its_length() {
        let header = ReplayHeader {
            version: 2,
            tick_rate: 60,
            ..ReplayHeader::default()
        };
        let bytes = SyntheticReplay::new(&header)
            .inputs(0, &[])
            .raw(&[0x01, 1, 4, 0, 9])
            .inputs(2, &[])
            .finish();
        let path = unique_temp("replay-v2").with_extension("do27replay");
        std::fs::write(&path, bytes).expect("write");
        let mut world = World::new();
        let err = replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect_err("refused");
        assert!(err.message.contains("[v2_full]"), "{}", err.message);
        assert!(
            err.message.contains("version 3"),
            "the message says what to do: {}",
            err.message
        );
        world.insert_resource(ReplaySim::with_codec(V2Codec));
        let out =
            replay_load(&mut world, &json!({ "path": path.to_string_lossy() })).expect("with the codec");
        assert_eq!(out["counts"]["fulls"], 1);
        assert_eq!(
            out["verify"]["hash_unchecked"], 1,
            "a version 2 full has no hash to check"
        );
        let at = replay_seek(&mut world, &json!({ "tick": 1 })).expect("seek");
        assert_eq!(at["snapshot"]["entities"][0]["id"], 4);
        let _ = std::fs::remove_file(&path);
    }
}
