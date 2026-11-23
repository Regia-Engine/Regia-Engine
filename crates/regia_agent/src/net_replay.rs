// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the network recorder and replay player (spec 103 § 9, `book/agents/net-replay.md`,
//! ADR-3021): `net.replay_record_start`, `net.replay_record_stop`, `net.replay_ring_export`, `net.replay_open`,
//! `net.replay_seek`, `net.replay_play`, `net.replay_close`, `net.replay_state`.
//!
//! Errors are `F0002` with `data.reason`: `net_not_server`, `net_not_running`, `recorder_busy`, `recorder_idle`,
//! `recorder_view_conflict`, `recorder_view_invalid`, `recorder_config_invalid`, `ring_idle`, `ring_empty`,
//! `netdemo_exists`, `netdemo_io`, `netdemo_not_a_demo`, `netdemo_version`, `netdemo_protocol_mismatch`,
//! `netdemo_truncated`, `netdemo_corrupt`, `netdemo_session_active`, `replay_closed`, `replay_seek_range`,
//! `path_outside_project`.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_framework::demo_scrub::tick_at_fraction;
use regia_framework::net_replay::{
    NetReplayPlayer, NetReplayState, ReplayCamera, ReplayOptions, close_replay, open_replay, replay_state,
};
use regia_net::model::{NetId, NetTick};
use regia_net::recorder::{
    FileStatus, NetDemoError, RecorderConfig, RecorderStatus, RecorderView, RecordingSummary, RingConfig,
    RingStatus, export_ring, recorder_status, start_recording, start_ring, stop_recording, stop_ring,
};
use regia_net::runtime::NetNode;
use regia_net::session::PeerId;
use serde_json::{Value, json};

use crate::net_session::f0002;
use crate::protocol::RpcError;

fn hint(code: &str) -> &'static str {
    match code {
        "net_not_server" => "record on the server's world: host a session or set NetMode to a server mode",
        "recorder_busy" => {
            "net.replay_record_stop first (a recording a cap ended waits there to be collected)"
        }
        "recorder_idle" | "ring_idle" => "net.replay_record_start first",
        "recorder_view_conflict" => {
            "the file and the ring share a connection: use the running view or stop it first"
        }
        "recorder_view_invalid" => {
            "`view` is \"everything\" or \"player:<peer>\" with a connected peer (net.peers)"
        }
        "recorder_config_invalid" => "see the method's parameters in book/agents/net-replay.md",
        "ring_empty" => "the ring holds a whole segment after its first send tick: step or wait, then export",
        "netdemo_exists" => "choose another path or pass overwrite: true",
        "netdemo_io" => "check the path and the disk",
        "netdemo_not_a_demo" | "netdemo_truncated" | "netdemo_corrupt" => {
            "the file is not a whole .netdemo: a recording appears at its path only when it closes"
        }
        "netdemo_version" => "this build reads the format_version in docs/formats/netdemo.md",
        "netdemo_protocol_mismatch" => {
            "play it with the build that recorded it: components and messages must be registered the same, in the same order"
        }
        "netdemo_session_active" => "leave the session first (net.disconnect or net.stop)",
        "replay_seek_range" => "net.replay_state gives first_tick and last_tick",
        _ => "see book/agents/net-replay.md",
    }
}

fn demo_error(e: &NetDemoError) -> RpcError {
    f0002(e.code(), e.to_string(), hint(e.code()))
}

fn running(world: &World) -> Result<(), RpcError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net runtime",
            "add regia_net::NetPlugin (or call regia_net::runtime::ensure_resources) and set a NetMode",
        ));
    }
    Ok(())
}

fn tick_or_null(t: Option<NetTick>) -> Value {
    t.map_or(Value::Null, |t| json!(t.0))
}

fn file_json(f: &FileStatus) -> Value {
    json!({
        "path": f.path.display().to_string(),
        "bytes": f.bytes,
        "ticks": f.ticks,
        "keyframes": f.keyframes,
        "first_tick": tick_or_null(f.first_tick),
        "last_tick": tick_or_null(f.last_tick),
        "ended": f.ended.map(|r| r.as_str()),
        "error": f.error,
    })
}

fn ring_json(r: &RingStatus) -> Value {
    json!({
        "max_bytes": r.max_bytes,
        "used_bytes": r.used_bytes,
        "peak_bytes": r.peak_bytes,
        "segments": r.segments,
        "first_tick": tick_or_null(r.first_tick),
        "last_tick": tick_or_null(r.last_tick),
        "overflows": r.overflows,
    })
}

fn recorder_json(s: &RecorderStatus) -> Value {
    json!({
        "running": s.view.is_some(),
        "view": s.view.map(RecorderView::label),
        "view_peer_gone": s.view_peer_gone,
        "file": s.file.as_ref().map(file_json),
        "ring": s.ring.as_ref().map(ring_json),
        "keyframes_skipped": s.keyframes_skipped,
        "ticks_skipped": s.ticks_skipped,
        "scratch_grows": s.scratch_grows,
        "ticks": s.ticks,
    })
}

fn summary_json(s: &RecordingSummary) -> Value {
    json!({
        "path": s.path.display().to_string(),
        "bytes": s.bytes,
        "ticks": s.ticks,
        "keyframes": s.keyframes,
        "first_tick": s.first_tick.0,
        "last_tick": s.last_tick.0,
        "reason": s.reason.as_str(),
    })
}

fn player_json(s: &NetReplayState) -> Value {
    let camera = match s.camera {
        ReplayCamera::Free => json!("free"),
        ReplayCamera::Follow(id) => json!({ "follow": id.0 }),
    };
    json!({
        "tick": s.tick,
        "first_tick": s.first_tick,
        "last_tick": s.last_tick,
        "tick_hz": s.tick_hz,
        "seconds": s.seconds,
        "total_seconds": s.total_seconds,
        "playing": s.playing,
        "ended": s.ended,
        "seeking": s.seeking,
        "speed": s.speed,
        "generation": s.generation,
        "camera": camera,
        "follow_entity": s.follow_entity.map(Entity::to_bits),
        "keyframes": s.keyframes,
        "resync_unanswered": s.resync_unanswered,
        "error": s.error,
    })
}

fn f32_param(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| Some(n as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

fn u64_param(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn bool_param(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

/// A request's path is confined like every other agent path (ADR-1816, `book/agents/safety.md`): this method writes
/// and replaces files.
fn path_param(world: &World, params: &Value, key: &str) -> Result<Option<PathBuf>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let text = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a path")))?;
            crate::req_path::allowed_path(world, key, text).map(Some)
        }
    }
}

fn view_param(params: &Value) -> Result<RecorderView, RpcError> {
    let Some(v) = params.get("view").filter(|v| !v.is_null()) else {
        return Ok(RecorderView::Everything);
    };
    let bad = || {
        f0002(
            "recorder_view_invalid",
            format!("`view` {v} is not a view"),
            hint("recorder_view_invalid"),
        )
    };
    let text = v.as_str().ok_or_else(bad)?;
    if text == "everything" {
        return Ok(RecorderView::Everything);
    }
    let peer = text
        .strip_prefix("player:")
        .and_then(|p| p.parse::<u32>().ok())
        .ok_or_else(bad)?;
    Ok(RecorderView::Player(PeerId(peer)))
}

/// `net.replay_record_start`: `path` records a file, `ring_seconds` keeps a ring; both share one connection.
pub(crate) fn record_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let path = path_param(world, params, "path")?;
    let ring_seconds = f32_param(params, "ring_seconds")?;
    if path.is_none() && ring_seconds.is_none() {
        return Err(RpcError::invalid_params(
            "give `path` to record a file, `ring_seconds` to keep a ring, or both",
        ));
    }
    // Every parameter first: a bad one starts nothing.
    let view = view_param(params)?;
    let keyframe = f32_param(params, "keyframe_s")?;
    let d = RecorderConfig::default();
    let file_config = RecorderConfig {
        view,
        keyframe_every_s: keyframe.unwrap_or(d.keyframe_every_s),
        max_bytes: u64_param(params, "max_bytes")?.unwrap_or(d.max_bytes),
        max_seconds: f32_param(params, "max_seconds")?.unwrap_or(d.max_seconds),
        level: crate::params::opt_str(params, "level")?
            .unwrap_or_default()
            .to_string(),
        overwrite: bool_param(params, "overwrite")?.unwrap_or(false),
    };
    let d = RingConfig::default();
    let ring_bytes = match u64_param(params, "ring_bytes")? {
        None => d.max_bytes,
        // Past the address space is as much over the limit as anything `start_ring` refuses.
        Some(b) => usize::try_from(b).unwrap_or(usize::MAX),
    };
    if let Some(path) = path.as_ref() {
        start_recording(world, path, file_config).map_err(|e| demo_error(&e))?;
    }
    if let Some(seconds) = ring_seconds {
        let started = start_ring(
            world,
            RingConfig {
                view,
                seconds,
                max_bytes: ring_bytes,
                keyframe_every_s: keyframe.unwrap_or(d.keyframe_every_s),
            },
        );
        if let Err(e) = started {
            // All or nothing: the file this call just started goes away with the refusal.
            if path.is_some() {
                let _ = stop_recording(world);
            }
            return Err(demo_error(&e));
        }
    }
    Ok(recorder_json(&recorder_status(world)))
}

/// `net.replay_record_stop { target? }`: `file`, `ring` or `both` (default: whatever runs).
pub(crate) fn record_stop(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let target = crate::params::opt_str(params, "target")?.unwrap_or("both");
    if !matches!(target, "file" | "ring" | "both") {
        return Err(RpcError::invalid_params("`target` is file, ring or both"));
    }
    let status = recorder_status(world);
    if target == "both" && status.file.is_none() && status.ring.is_none() {
        return Err(demo_error(&NetDemoError::Idle));
    }
    let mut out = json!({});
    // A file that failed is reported, after the ring has stopped too: `both` stops both.
    let mut file_error = None;
    if target == "file" || (target == "both" && status.file.is_some()) {
        match stop_recording(world) {
            Ok(summary) => out["file"] = summary_json(&summary),
            Err(e) => file_error = Some(e),
        }
    }
    if target == "ring" || (target == "both" && status.ring.is_some()) {
        let ring = stop_ring(world).map_err(|e| demo_error(&e))?;
        out["ring"] = ring_json(&ring);
    }
    if let Some(e) = file_error {
        return Err(demo_error(&e));
    }
    Ok(out)
}

/// `net.replay_ring_export { path, seconds?, overwrite? }`.
pub(crate) fn ring_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let path =
        path_param(world, params, "path")?.ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let seconds = f32_param(params, "seconds")?;
    let overwrite = bool_param(params, "overwrite")?.unwrap_or(false);
    let summary = export_ring(world, &path, seconds, overwrite).map_err(|e| demo_error(&e))?;
    Ok(summary_json(&summary))
}

/// `net.replay_open { path, paused?, interpolate? }`.
pub(crate) fn open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let path =
        path_param(world, params, "path")?.ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let d = ReplayOptions::default();
    let options = ReplayOptions {
        paused: bool_param(params, "paused")?.unwrap_or(d.paused),
        interpolate: bool_param(params, "interpolate")?.unwrap_or(d.interpolate),
        level_restore: d.level_restore,
    };
    open_replay(world, Path::new(&path), options).map_err(|e| demo_error(&e))?;
    let player = world.resource::<NetReplayPlayer>();
    let h = player.header();
    let head = json!({
        "protocol_hash": format!("{:016x}", h.protocol_hash),
        "level": h.level,
        "engine_version": h.engine_version,
        "view": h.view.label(),
        "send_every_ticks": h.send_every_ticks,
        "keyframe_every_ticks": h.keyframe_every_ticks,
        "components": h.components.len(),
        "messages": h.messages.len(),
    });
    let state = replay_state(world)
        .map(|s| player_json(&s))
        .unwrap_or(Value::Null);
    Ok(json!({ "header": head, "player": state }))
}

fn player_mut(world: &mut World) -> Result<Mut<'_, NetReplayPlayer>, RpcError> {
    world.get_resource_mut::<NetReplayPlayer>().ok_or_else(|| {
        f0002(
            "replay_closed",
            "no replay is open",
            "net.replay_open first, in a world with no running session",
        )
    })
}

fn state_of(world: &World) -> Value {
    replay_state(world).map_or(Value::Null, |s| player_json(&s))
}

/// `net.replay_seek { tick | seconds | fraction }`. It lands on the next frame: `net.replay_state` then says so.
pub(crate) fn seek(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let tick = u64_param(params, "tick")?;
    let seconds = params.get("seconds").filter(|v| !v.is_null());
    let fraction = f32_param(params, "fraction")?;
    let state = replay_state(world);
    let mut player = player_mut(world)?;
    let result = match (tick, seconds, fraction) {
        (Some(t), None, None) => player.seek(u32::try_from(t).unwrap_or(u32::MAX)),
        (None, Some(s), None) => {
            let s = s
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params("`seconds` must be a number"))?;
            player.seek_seconds(s)
        }
        (None, None, Some(f)) => {
            let (first, last) = state.map_or((0, 0), |s| (s.first_tick, s.last_tick));
            let steps = u64::from(last - first);
            player.seek(first + tick_at_fraction(f, steps) as u32)
        }
        _ => {
            return Err(RpcError::invalid_params(
                "give exactly one of `tick`, `seconds` or `fraction`",
            ));
        }
    };
    result.map_err(|e| demo_error(&e))?;
    Ok(state_of(world))
}

/// `net.replay_play { playing?, speed?, follow? }`: run or pause, set the speed, set the camera (`follow` is a `NetId`,
/// or null for free).
pub(crate) fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let playing = bool_param(params, "playing")?.unwrap_or(true);
    let speed = f32_param(params, "speed")?;
    // Every parameter first: a bad one changes nothing.
    if speed.is_some_and(|s| !(s.is_finite() && s > 0.0)) {
        return Err(RpcError::invalid_params("`speed` must be above 0"));
    }
    let camera = match params.get("follow") {
        None => None,
        Some(Value::Null) => Some(ReplayCamera::Free),
        Some(v) => {
            let id = v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| {
                RpcError::invalid_params("`follow` must be a net id, or null for a free camera")
            })?;
            Some(ReplayCamera::Follow(NetId(id)))
        }
    };
    let mut player = player_mut(world)?;
    if playing {
        player.play();
    } else {
        player.pause();
    }
    if let Some(speed) = speed {
        player.set_speed(speed);
    }
    if let Some(camera) = camera {
        player.set_camera(camera);
    }
    Ok(state_of(world))
}

/// `net.replay_close`: ends the open replay; the copies go and the world's mode comes back.
pub(crate) fn close(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    player_mut(world)?;
    close_replay(world);
    Ok(json!({ "closed": true }))
}

/// `net.replay_state`: the recorder (on a server) and the player (with a replay open).
pub(crate) fn state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    Ok(json!({
        "recorder": recorder_json(&recorder_status(world)),
        "player": state_of(world),
    }))
}

#[cfg(test)]
mod tests {
    use regia_net::model::{LocalPeer, NetMode};
    use regia_net::replication::{ReplicateOptions, Replicated, register_replicated};
    use regia_net::*;

    use super::*;

    #[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
    struct Health {
        hp: i32,
    }

    fn setup(world: &mut World) {
        register_replicated::<Health>(world, ReplicateOptions::default());
    }

    fn reason(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn temp_dir() -> PathBuf {
        regia_test_utils::unique_temp_dir("agent_net_replay")
    }

    fn server() -> regia_net::testing::LoopbackHub {
        let mut hub = regia_net::testing::LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
        hub.server.world.spawn((Replicated::default(), Health { hp: 3 }));
        hub
    }

    fn client_world() -> World {
        let mut w = World::new();
        w.insert_resource(NetMode::Client);
        w.insert_resource(LocalPeer(None));
        setup(&mut w);
        regia_net::runtime::ensure_resources(&mut w);
        w
    }

    /// Spec 103 § 12.14: every method round-trips: record a file and a ring, export, open, seek, play, read.
    #[test]
    fn every_method_round_trips() {
        let dir = temp_dir();
        let file = dir.join("a.netdemo");
        let cut = dir.join("cut.netdemo");
        let mut hub = server();
        let w = &mut hub.server.world;
        let status = record_start(
            w,
            &json!({ "path": file, "ring_seconds": 2.0, "ring_bytes": 1 << 20, "keyframe_s": 0.1, "level": "arena" }),
        )
        .expect("starts");
        assert_eq!(status["running"], true);
        assert_eq!(status["view"], "everything");
        assert_eq!(status["file"]["ticks"], 0);
        hub.run(30);
        let w = &mut hub.server.world;
        let s = state(w, &json!({})).expect("state");
        assert_eq!(s["recorder"]["file"]["keyframes"], 5);
        assert!(s["recorder"]["ring"]["used_bytes"].as_u64().expect("bytes") > 0);
        assert_eq!(s["player"], Value::Null);
        let exported = ring_export(w, &json!({ "path": cut, "seconds": 0.2 })).expect("exports");
        assert_eq!(exported["reason"], "stopped");
        let stopped = record_stop(w, &json!({})).expect("stops");
        assert_eq!(stopped["file"]["ticks"], 30);
        assert_eq!(stopped["file"]["reason"], "stopped");
        assert!(stopped["ring"]["peak_bytes"].as_u64().expect("bytes") > 0);

        let mut world = client_world();
        let opened = open(
            &mut world,
            &json!({ "path": file, "paused": true, "interpolate": false }),
        )
        .expect("opens");
        assert_eq!(opened["header"]["level"], "arena");
        assert_eq!(opened["header"]["components"], 1);
        assert_eq!(opened["player"]["playing"], false);
        let first = opened["player"]["first_tick"].as_u64().expect("tick");
        let seeking = seek(&mut world, &json!({ "tick": first + 10 })).expect("seeks");
        assert_eq!(seeking["seeking"], true);
        seek(&mut world, &json!({ "seconds": 0.1 })).expect("seeks");
        seek(&mut world, &json!({ "fraction": 0.5 })).expect("seeks");
        let played = play(&mut world, &json!({ "speed": 2.0, "follow": 1 })).expect("plays");
        assert_eq!(played["playing"], true);
        assert_eq!(played["speed"], 2.0);
        assert_eq!(played["camera"]["follow"], 1);
        let free = play(&mut world, &json!({ "playing": false, "follow": null })).expect("pauses");
        assert_eq!(free["playing"], false);
        assert_eq!(free["camera"], "free");
        let s = state(&mut world, &json!({})).expect("state");
        assert_eq!(s["player"]["keyframes"], 5);
        assert_eq!(s["recorder"]["running"], false);
        // The export opens too.
        let mut world = client_world();
        open(&mut world, &json!({ "path": cut })).expect("the ring's export opens");
    }

    /// Spec 103 § 12.14: each code is produced here, by the call that produces it for an agent.
    #[test]
    fn every_reason_is_produced() {
        let dir = temp_dir();
        let code = |r: Result<Value, RpcError>| reason(&r.expect_err("an error"));
        let mut empty = World::new();
        assert_eq!(code(state(&mut empty, &json!({}))), "net_not_running");
        let mut client = client_world();
        let file = dir.join("a.netdemo");
        assert_eq!(
            code(record_start(&mut client, &json!({ "path": file }))),
            "net_not_server"
        );
        assert_eq!(code(seek(&mut client, &json!({ "tick": 1 }))), "replay_closed");
        assert_eq!(code(play(&mut client, &json!({}))), "replay_closed");

        let mut hub = server();
        let w = &mut hub.server.world;
        assert_eq!(
            code(record_stop(w, &json!({ "target": "file" }))),
            "recorder_idle"
        );
        assert_eq!(code(record_stop(w, &json!({ "target": "ring" }))), "ring_idle");
        assert_eq!(code(ring_export(w, &json!({ "path": file }))), "ring_idle");
        assert_eq!(
            code(record_start(w, &json!({ "path": file, "view": "nobody" }))),
            "recorder_view_invalid"
        );
        assert_eq!(
            code(record_start(w, &json!({ "path": file, "view": "player:7" }))),
            "recorder_view_invalid"
        );
        assert_eq!(
            code(record_start(w, &json!({ "path": file, "keyframe_s": -1.0 }))),
            "recorder_config_invalid"
        );
        let taken = dir.join("taken.netdemo");
        std::fs::write(&taken, b"x").expect("write");
        assert_eq!(code(record_start(w, &json!({ "path": taken }))), "netdemo_exists");
        let nowhere = dir.join("no/dir/a.netdemo");
        assert_eq!(code(record_start(w, &json!({ "path": nowhere }))), "netdemo_io");
        record_start(w, &json!({ "ring_seconds": 1.0 })).expect("a ring");
        assert_eq!(code(ring_export(w, &json!({ "path": file }))), "ring_empty");
        assert_eq!(
            code(record_start(w, &json!({ "ring_seconds": 1.0 }))),
            "recorder_busy"
        );
        hub.add_client();
        let w = &mut hub.server.world;
        assert_eq!(
            code(record_start(w, &json!({ "path": file, "view": "player:1" }))),
            "recorder_view_conflict"
        );
        record_stop(w, &json!({})).expect("stops");

        // Files.
        let mut world = client_world();
        let junk = dir.join("junk.netdemo");
        std::fs::write(&junk, b"not a recording").expect("write");
        assert_eq!(
            code(open(&mut world, &json!({ "path": junk }))),
            "netdemo_not_a_demo"
        );
        assert_eq!(
            code(open(&mut world, &json!({ "path": dir.join("gone") }))),
            "netdemo_io"
        );

        let good = dir.join("good.netdemo");
        let mut hub = server();
        record_start(&mut hub.server.world, &json!({ "path": good })).expect("starts");
        hub.run(10);
        record_stop(&mut hub.server.world, &json!({})).expect("stops");
        let bytes = std::fs::read(&good).expect("read");
        let cut = dir.join("cut.netdemo");
        std::fs::write(&cut, &bytes[..bytes.len() - 5]).expect("write");
        assert_eq!(
            code(open(&mut world, &json!({ "path": cut }))),
            "netdemo_truncated"
        );
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 30] ^= 1;
        let flipped = dir.join("flipped.netdemo");
        std::fs::write(&flipped, &bad).expect("write");
        assert_eq!(
            code(open(&mut world, &json!({ "path": flipped }))),
            "netdemo_corrupt"
        );
        let mut future = bytes.clone();
        future[8] = 9;
        let newer = dir.join("newer.netdemo");
        std::fs::write(&newer, &future).expect("write");
        assert_eq!(
            code(open(&mut world, &json!({ "path": newer }))),
            "netdemo_version"
        );
        let mut other = World::new();
        other.insert_resource(NetMode::Client);
        regia_net::runtime::ensure_resources(&mut other);
        assert_eq!(
            code(open(&mut other, &json!({ "path": good }))),
            "netdemo_protocol_mismatch"
        );
        let mut live = server();
        live.add_client();
        assert_eq!(
            code(open(&mut live.server.world, &json!({ "path": good }))),
            "netdemo_session_active"
        );
        open(&mut world, &json!({ "path": good })).expect("opens");
        assert_eq!(
            code(seek(&mut world, &json!({ "tick": 100_000 }))),
            "replay_seek_range"
        );
    }

    /// ADR-1816: requests are untrusted, and these methods write and replace files.
    #[test]
    fn a_path_outside_the_allowed_roots_is_refused() {
        let mut hub = server();
        let w = &mut hub.server.world;
        let outside = if cfg!(windows) {
            "C:\\x.netdemo"
        } else {
            "/etc/x.netdemo"
        };
        for path in [outside, "../x.netdemo", "a/../../x.netdemo"] {
            let code = |r: Result<Value, RpcError>| reason(&r.expect_err(path));
            assert_eq!(
                code(record_start(w, &json!({ "path": path }))),
                "path_outside_project"
            );
            assert_eq!(
                code(ring_export(w, &json!({ "path": path }))),
                "path_outside_project"
            );
            let mut world = client_world();
            assert_eq!(
                code(open(&mut world, &json!({ "path": path }))),
                "path_outside_project"
            );
        }
        assert_eq!(state(w, &json!({})).expect("state")["recorder"]["running"], false);
    }

    /// A close that failed reaches a plain `record_stop`, which then frees the recorder for the next start.
    #[test]
    fn a_failed_close_is_reported_and_does_not_lock_the_recorder() {
        let dir = temp_dir();
        let file = dir.join("a.netdemo");
        let mut hub = server();
        record_start(
            &mut hub.server.world,
            &json!({ "path": file, "max_bytes": 4096, "keyframe_s": 0.1, "overwrite": true }),
        )
        .expect("starts");
        // The size cap ends it, and the rename onto this directory then fails for real.
        std::fs::create_dir(&file).expect("a directory at the path");
        for _ in 0..4000 {
            hub.run(1);
            let w = &mut hub.server.world;
            let mut q = w.query::<&mut Health>();
            for mut h in q.iter_mut(w) {
                h.hp += 1;
            }
            if recorder_status(w).file.is_some_and(|f| f.ended.is_some()) {
                break;
            }
        }
        let w = &mut hub.server.world;
        let status = recorder_status(w);
        assert_eq!(
            status.file.as_ref().and_then(|f| f.ended).map(|r| r.as_str()),
            Some("size_cap"),
            "{status:?}"
        );
        let err = record_stop(w, &json!({})).expect_err("the rename failed");
        assert_eq!(reason(&err), "netdemo_io");
        let again = dir.join("b.netdemo");
        record_start(w, &json!({ "path": again })).expect("the recorder is free again");
        assert_eq!(
            reason(&record_start(w, &json!({ "path": dir.join("c") })).expect_err("busy")),
            "recorder_busy"
        );
    }

    /// Each parameter is read before anything starts or changes.
    #[test]
    fn a_refused_call_leaves_nothing_changed() {
        let dir = temp_dir();
        let file = dir.join("a.netdemo");
        let mut hub = server();
        let w = &mut hub.server.world;
        assert!(
            record_start(
                w,
                &json!({ "path": file, "ring_seconds": 1.0, "ring_bytes": "many" })
            )
            .is_err()
        );
        assert_eq!(recorder_status(w).view, None, "no file started");
        for (bytes, seconds) in [(u64::MAX, 1.0), (1 << 40, 1.0), (16, 1.0)] {
            let e = record_start(
                w,
                &json!({ "path": file, "ring_seconds": seconds, "ring_bytes": bytes }),
            )
            .expect_err("refused");
            assert_eq!(reason(&e), "recorder_config_invalid", "{bytes}");
            assert_eq!(recorder_status(w).view, None, "all or nothing: {bytes}");
        }
        assert!(!file.exists());
        // A window far past any memory asks for no more than the ring's bound.
        record_start(
            w,
            &json!({ "ring_seconds": 1e30, "keyframe_s": 0.0001, "ring_bytes": 4096 }),
        )
        .expect("bounded");
        record_stop(w, &json!({})).expect("stops");

        let mut hub = server();
        let w = &mut hub.server.world;
        record_start(w, &json!({ "path": file })).expect("starts");
        hub.run(5);
        record_stop(&mut hub.server.world, &json!({})).expect("stops");
        let mut world = client_world();
        open(&mut world, &json!({ "path": file, "paused": true })).expect("opens");
        assert!(play(&mut world, &json!({ "speed": -1.0 })).is_err());
        assert_eq!(
            state_of(&world)["playing"],
            false,
            "a bad speed does not start it"
        );
        assert!(play(&mut world, &json!({ "follow": "someone" })).is_err());
        assert_eq!(state_of(&world)["playing"], false, "nor does a bad camera");
    }

    /// `net.replay_close` ends it, and says so when there is nothing to end; a server world is never opened.
    #[test]
    fn close_ends_the_replay_and_a_server_is_refused() {
        let dir = temp_dir();
        let file = dir.join("a.netdemo");
        let mut hub = server();
        record_start(&mut hub.server.world, &json!({ "path": file })).expect("starts");
        hub.run(5);
        let w = &mut hub.server.world;
        assert_eq!(
            reason(&open(w, &json!({ "path": dir.join("nothing.netdemo") })).expect_err("refused")),
            "netdemo_io"
        );
        record_stop(w, &json!({})).expect("stops");
        // No client, no session, and still a server: the reset would take its entities.
        let held = w.query::<&Health>().iter(w).count();
        assert_eq!(
            reason(&open(w, &json!({ "path": file })).expect_err("refused")),
            "netdemo_session_active"
        );
        assert_eq!(w.query::<&Health>().iter(w).count(), held);

        let mut world = client_world();
        assert_eq!(
            reason(&close(&mut world, &json!({})).expect_err("no replay")),
            "replay_closed"
        );
        open(&mut world, &json!({ "path": file })).expect("opens");
        assert_eq!(close(&mut world, &json!({})).expect("closes")["closed"], true);
        assert_eq!(state_of(&world), Value::Null);
        assert_eq!(
            *world.resource::<NetMode>(),
            NetMode::Client,
            "the mode came back"
        );
    }

    #[test]
    fn a_bad_parameter_is_invalid_params() {
        let mut hub = server();
        let w = &mut hub.server.world;
        assert!(record_start(w, &json!({})).is_err(), "no target");
        assert!(record_stop(w, &json!({ "target": "elsewhere" })).is_err());
        assert!(ring_export(w, &json!({})).is_err());
        let mut world = client_world();
        assert!(open(&mut world, &json!({})).is_err());
    }
}
