// SPDX-License-Identifier: MPL-2.0
//! Lockstep match replays on the agent protocol (ADR-8513; `book/agents/net-lsreplay.md`):
//! `net.replays` lists a `.lsreplay` library folder (newest first, damaged files apart) and
//! `net.replay_audit` re-runs one replay through the game's `DesyncReplay` hook and checks every
//! recorded hash bit for bit, as `regia net replay-audit` does.
//!
//! Codes: `replay_not_found`, `replay_version_newer`, `replay_truncated`, `replay_game_mismatch`,
//! `replay_no_hook`, `replay_too_large`, `replay_invalid`, `replay_unknown_record`,
//! `replay_not_lsreplay`, `replay_hook_failed`, `replay_io` (the codes of
//! `regia_net::lockstep::replay::LsReplayError`), plus `F0002` for a path outside the project.

use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::desync::ReplayRegistry;
use regia_net::lockstep::replay::{
    DEFAULT_REPLAY_DIR, LsReplayError, LsReplayReader, ReplayInfo, ReplayLibrary, audit_with, read_file,
};
use serde_json::{Value, json};

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

/// The protocol error of a replay error: its own code, a JSON-RPC number and a hint.
fn rpc(err: &LsReplayError) -> RpcError {
    let (number, hint) = match err {
        LsReplayError::NotFound(_) => (NOT_FOUND, "net.replays lists the replays of a folder"),
        LsReplayError::VersionNewer { .. } => (UNSUPPORTED, "open it with a newer build of the engine"),
        LsReplayError::GameMismatch { .. } | LsReplayError::NoHook { .. } => (
            UNSUPPORTED,
            "audit it with the build of the game that recorded it (its tool binary registers the hook)",
        ),
        LsReplayError::Io(_) | LsReplayError::HookFailed { .. } => {
            (INTERNAL_ERROR, "see the message; the file itself read")
        }
        _ => (
            INVALID_PARAMS,
            "the file is damaged or not a .lsreplay; net.replays lists it under `damaged`",
        ),
    };
    RpcError::new(number, err.code(), err.to_string(), hint)
}

fn row(info: &ReplayInfo) -> Value {
    json!({
        "file_name": info.file_name,
        "date": info.date,
        "length": info.length,
        "game_id": info.game_id,
        "protocol_version": info.protocol_version,
        "engine_version": info.engine_version,
        "seats": info.seats,
        "tick_rate": info.tick_rate,
        "seed": info.seed,
        "records": info.records,
        "aborted": info.aborted,
    })
}

/// `net.replays {dir?}`: the library folder (project-relative; the game's published folder, else
/// `saved/replays`), readable replays newest first and damaged files apart.
pub(crate) fn replays(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dir = match params.get("dir") {
        None | Some(Value::Null) => world
            .get_resource::<NetAgentState>()
            .and_then(NetAgentState::replay_dir)
            .unwrap_or(DEFAULT_REPLAY_DIR)
            .to_owned(),
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`dir` must be a non-empty project-relative path",
            ));
        }
    };
    let path = crate::req_path::project_path(world, "dir", &dir)?;
    let list = ReplayLibrary::new(&path).list().map_err(|e| rpc(&e))?;
    let damaged: Vec<Value> = list
        .damaged
        .iter()
        .map(|d| json!({ "file_name": d.file_name, "code": d.code, "reason": d.reason }))
        .collect();
    Ok(json!({
        "dir": dir,
        "replays": list.replays.iter().map(row).collect::<Vec<_>>(),
        "damaged": damaged,
    }))
}

/// `net.replay_audit {path}`: re-runs the replay through the hook registered for its game (the
/// game's hooks, `NetAgentState::set_replay_hooks`, then the engine's sample) and compares every
/// recorded hash bit for bit.
pub(crate) fn replay_audit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let rel = crate::params::opt_str(params, "path")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`path` (a project-relative .lsreplay file) is required"))?;
    let path = crate::req_path::project_path(world, "path", rel)?;
    if !path.exists() {
        return Err(rpc(&LsReplayError::NotFound(rel.to_owned())));
    }
    // A FIFO or a device is refused before it is read.
    let root = crate::req_path::project_root(world);
    crate::req_path::project_file_in(&root, "path", rel)?;
    let bytes = read_file(&path).map_err(|e| rpc(&e))?;
    let game = LsReplayReader::new(&bytes)
        .map_err(|e| rpc(&e))?
        .header()
        .game_id
        .clone();
    let builtin = ReplayRegistry::with_builtin();
    let registry = world
        .get_resource::<NetAgentState>()
        .and_then(NetAgentState::replay_hooks)
        .filter(|r| r.games().contains(&game.as_str()))
        .unwrap_or(&builtin);
    let audit = audit_with(&bytes, registry).map_err(|e| rpc(&e))?;
    let mut value = serde_json::to_value(&audit).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut value {
        map.insert("path".into(), rel.into());
        map.insert("bit_exact".into(), audit.bit_exact().into());
        map.insert("summary".into(), json!(audit.summary()));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use regia_net::lockstep::replay::{LSREPLAY_MAGIC, sample_replay};
    use regia_test_utils::temp::TempDir;

    use super::*;

    fn world(dir: &Path) -> World {
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(dir.to_path_buf()),
        });
        world
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        crate::dispatch::call_in_process(world, method, params).expect("answers at once")
    }

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn put(dir: &Path, rel: &str, bytes: &[u8]) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dir");
        }
        std::fs::write(path, bytes).expect("write");
    }

    /// Dispatcher end to end: `net.replay_audit` audits a clean replay bit-exact and names a
    /// flipped input's tick and subsystem.
    #[test]
    fn net_replay_audit_names_the_flipped_tick() {
        let dir = TempDir::new("agent-replay-audit");
        put(
            &dir,
            "saved/replays/clean.lsreplay",
            &sample_replay(3, 90, 30, 1, None),
        );
        put(
            &dir,
            "saved/replays/flip.lsreplay",
            &sample_replay(3, 90, 30, 1, Some((44, 1))),
        );
        let mut world = world(&dir);
        let clean = call(
            &mut world,
            "net.replay_audit",
            json!({"path": "saved/replays/clean.lsreplay"}),
        )
        .expect("clean");
        assert_eq!(clean["bit_exact"], true, "{clean}");
        assert_eq!(clean["frames"], 90);
        let flip = call(
            &mut world,
            "net.replay_audit",
            json!({"path": "saved/replays/flip.lsreplay"}),
        )
        .expect("flip");
        assert_eq!(flip["bit_exact"], false);
        assert_eq!(flip["first_diverging_tick"], 44);
        assert_eq!(flip["subsystem"], "units");
        assert_eq!(flip["header"]["game_id"], "regia.desync_sample");
    }

    /// Dispatcher end to end, one case per error code the brief names (`replay_not_found`,
    /// `replay_version_newer`, `replay_truncated`, `replay_game_mismatch`) plus a path outside
    /// the project, a missing `path` and a game without a hook.
    #[test]
    fn net_replay_audit_refuses_with_each_code() {
        let dir = TempDir::new("agent-replay-audit-codes");
        let good = sample_replay(2, 30, 10, 5, None);
        let mut newer = good.clone();
        newer[LSREPLAY_MAGIC.len()..LSREPLAY_MAGIC.len() + 4].copy_from_slice(&2u32.to_le_bytes());
        put(&dir, "r/newer.lsreplay", &newer);
        put(&dir, "r/short.lsreplay", &good[..good.len() - 1]);
        // Another version of the sample game: the header's protocol version differs.
        let mut reader = LsReplayReader::new(&good).expect("header");
        let mut header = reader.header().clone();
        header.protocol_version += 1;
        let mut writer =
            regia_net::lockstep::replay::LsReplayWriter::new(Vec::new(), header.clone(), Default::default())
                .expect("writer");
        while let Some(record) = reader.next_record().expect("record") {
            writer.write_record(&record).expect("record");
        }
        put(&dir, "r/older.lsreplay", &writer.into_inner());
        header.game_id = "studio.unknown".into();
        let other = regia_net::lockstep::replay::LsReplayWriter::new(Vec::new(), header, Default::default())
            .expect("writer")
            .into_inner();
        put(&dir, "r/other.lsreplay", &other);
        let mut world = world(&dir);
        for (path, expected) in [
            ("r/missing.lsreplay", "replay_not_found"),
            ("r/newer.lsreplay", "replay_version_newer"),
            ("r/short.lsreplay", "replay_truncated"),
            ("r/older.lsreplay", "replay_game_mismatch"),
            ("r/other.lsreplay", "replay_no_hook"),
            ("../outside.lsreplay", "F0002"),
        ] {
            let err = call(&mut world, "net.replay_audit", json!({ "path": path })).expect_err(path);
            assert_eq!(code(&err), expected, "{path}: {err:?}");
            assert!(!err.message.is_empty());
        }
        let err = call(&mut world, "net.replay_audit", json!({})).expect_err("no path");
        assert_eq!(code(&err), "F0002", "{err:?}");
        // A game's hook registered on the agent state is used for its game.
        let mut hooks = ReplayRegistry::new();
        hooks.register("regia.desync_sample", || {
            Box::new(regia_net::desync::sample::SampleSim::default())
        });
        let mut state = NetAgentState::default();
        state.set_replay_hooks(hooks);
        world.insert_resource(state);
        put(&dir, "r/good.lsreplay", &good);
        let ok = call(&mut world, "net.replay_audit", json!({"path": "r/good.lsreplay"})).expect("game hook");
        assert_eq!(ok["bit_exact"], true);
    }

    /// Dispatcher end to end: `net.replays` lists the folder newest first with damaged files
    /// apart (each with its code), defaults to the published folder, and refuses a path outside
    /// the project; an absent folder is an empty list.
    #[test]
    fn net_replays_lists_newest_first_and_damaged_apart() {
        let dir = TempDir::new("agent-replays");
        let mut world = world(&dir);
        let empty = call(&mut world, "net.replays", json!({})).expect("absent folder");
        assert_eq!(empty["dir"], DEFAULT_REPLAY_DIR);
        assert_eq!(empty["replays"], json!([]));
        let library = dir.join("matches");
        std::fs::create_dir_all(&library).expect("dir");
        for (i, name) in ["a.lsreplay", "b.lsreplay"].iter().enumerate() {
            let path = library.join(name);
            std::fs::write(&path, sample_replay(2, 20 + i as u64, 10, 10, None)).expect("write");
            let file = std::fs::File::options().write(true).open(&path).expect("open");
            file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100 + i as u64))
                .expect("mtime");
        }
        std::fs::write(library.join("c.lsreplay"), b"RGLSRPLY\x09\0\0\0").expect("write");
        let mut state = NetAgentState::default();
        state.set_replay_dir("matches");
        world.insert_resource(state);
        let list = call(&mut world, "net.replays", json!({})).expect("list");
        assert_eq!(list["dir"], "matches");
        let names: Vec<&str> = list["replays"]
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|r| r["file_name"].as_str())
            .collect();
        assert_eq!(names, ["b.lsreplay", "a.lsreplay"]);
        assert_eq!(list["damaged"][0]["code"], "replay_version_newer");
        let err = call(&mut world, "net.replays", json!({"dir": "/etc"})).expect_err("absolute");
        assert_eq!(code(&err), "F0002");
    }
}
