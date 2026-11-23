// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of a dedicated server's kept world (ADR-3354,
//! `book/agents/net-server-persistence.md`): `server.save_now`, `server.persistence_status` and
//! `server.restore`. They run in the server's world, where
//! `regia_framework::server_persistence::ServerPersistencePlugin` keeps its state.
//!
//! Errors are `F0002` with `data.reason` set to one of: `server_persistence_missing` (the plugin is
//! not in this app), `persistence_disabled` (`[persistence] enabled = false`, or the host has not
//! started), `persistence_write_failed`, `persistence_encode_failed`, `persistence_slot_unknown`,
//! `persistence_save_corrupt`, `confirmation_required`.

use regia_app::prelude::*;
use regia_framework::server_persistence::{self as persist, PersistError, SaveReason, ServerPersistence};
use serde_json::{Value, json};

use crate::net_session::f0002;
use crate::protocol::RpcError;

fn installed(world: &World) -> Result<(), RpcError> {
    if world.contains_resource::<ServerPersistence>() {
        Ok(())
    } else {
        Err(f0002(
            "server_persistence_missing",
            "this app has no server persistence",
            "add regia_framework::server_persistence::ServerPersistencePlugin to the server app",
        ))
    }
}

fn persist_error(e: PersistError) -> RpcError {
    let hint = match e.code {
        "persistence_disabled" => "set [persistence] enabled = true in server.toml and restart the server",
        "persistence_slot_unknown" => "server.persistence_status lists the slots (saves[].sequence)",
        "persistence_save_corrupt" => "pick an older slot; the world was not changed",
        "persistence_write_failed" | "persistence_encode_failed" => {
            "check the disk and the server log; the previous saves are untouched"
        }
        _ => "see book/agents/net-server-persistence.md",
    };
    f0002(e.code, e.message, hint)
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(format!("server persistence: {e}")))
}

/// `server.persistence_status`.
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let Some(status) = persist::status(world) else {
        return Err(f0002(
            "server_persistence_missing",
            "this app has no server persistence",
            "add regia_framework::server_persistence::ServerPersistencePlugin to the server app",
        ));
    };
    to_json(&status)
}

/// `server.save_now`.
pub fn save_now(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    installed(world)?;
    let report = persist::save_now(world, SaveReason::Manual).map_err(persist_error)?;
    to_json(&report)
}

/// `server.restore {slot, confirm: true}`.
pub fn restore(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let slot = params
        .get("slot")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`slot` (a save sequence number) is required"))?;
    if params.get("confirm").and_then(Value::as_bool) != Some(true) {
        return Err(f0002(
            "confirmation_required",
            "server.restore replaces the running world and every kept player state",
            "repeat with confirm: true once the owner agreed",
        ));
    }
    installed(world)?;
    let report = persist::restore_slot(world, slot).map_err(persist_error)?;
    let mut out = to_json(&report)?;
    if let Some(map) = out.as_object_mut() {
        map.insert("restored".into(), json!(true));
    }
    Ok(out)
}

/// The `server-players` save section (ADR-3354, `docs/formats/server-players.md`): two players, one
/// with pawn state. The loader decodes and validates it as a restore does and re-encodes it.
fn server_players_fixture() -> Vec<u8> {
    use std::collections::BTreeMap;
    let section = persist::PlayerSection {
        format_version: persist::PLAYER_SECTION_FORMAT_VERSION,
        level: "levels/island.level.ron".into(),
        tick: 18_000,
        sequence: 12,
        reason: SaveReason::Autosave,
        players: vec![
            persist::PlayerRecord {
                identity: "steam:76561190000000001".into(),
                name: "Ann".into(),
                stamp: 12,
                player: BTreeMap::from([("Inventory".to_owned(), vec![42, 0, 3])]),
                pawn: BTreeMap::from([("Transform".to_owned(), vec![0; 40])]),
            },
            persist::PlayerRecord {
                identity: "name:Bo".into(),
                name: "Bo".into(),
                stamp: 9,
                player: BTreeMap::new(),
                pawn: BTreeMap::new(),
            },
        ],
    };
    section.to_bytes().unwrap_or_default()
}

fn server_players_load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    persist::PlayerSection::from_bytes(bytes)
        .and_then(|s| s.to_bytes())
        .map_err(|e| e.to_string())
}

/// The fuzz targets of this module (`crate::fuzz_targets::targets`).
pub(crate) fn fuzz_targets() -> Vec<crate::fuzz_targets::FuzzTarget> {
    vec![crate::fuzz_targets::FuzzTarget {
        name: "server-players",
        format: Some("server-players"),
        format_version: persist::PLAYER_SECTION_FORMAT_VERSION,
        round_trip: true,
        input: crate::fuzz_targets::FuzzInput::Bytes {
            fixture: server_players_fixture,
            load: server_players_load,
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::server_persistence::ServerPersistencePlugin;
    use regia_net::server_host::PersistenceSettings;

    fn reason(r: Result<Value, RpcError>) -> String {
        let e = r.expect_err("an error");
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn every_method_round_trips_and_every_reason_is_produced() {
        let dir = regia_test_utils::unique_temp_dir("agent-server-persist");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        // No plugin.
        assert_eq!(
            reason(status(app.world_mut(), &json!({}))),
            "server_persistence_missing"
        );
        assert_eq!(
            reason(save_now(app.world_mut(), &json!({}))),
            "server_persistence_missing"
        );
        app.add_plugins(ServerPersistencePlugin);
        // Installed but the host has not turned it on.
        let st = status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(st["enabled"], json!(false));
        assert_eq!(
            reason(save_now(app.world_mut(), &json!({}))),
            "persistence_disabled"
        );
        assert_eq!(
            reason(restore(app.world_mut(), &json!({ "slot": 1 }))),
            "confirmation_required"
        );
        assert!(
            restore(app.world_mut(), &json!({ "confirm": true })).is_err(),
            "slot is required"
        );
        // On.
        let settings = PersistenceSettings {
            enabled: true,
            autosave_ticks: 0,
            ..PersistenceSettings::default()
        };
        persist::start(app.world_mut(), &settings, &dir, "levels/a.level.ron").expect("start");
        let saved = save_now(app.world_mut(), &json!({})).expect("save");
        assert_eq!(saved["sequence"], json!(1));
        assert_eq!(saved["reason"], json!("manual"));
        let st = status(app.world_mut(), &json!({})).expect("status");
        assert_eq!(st["enabled"], json!(true));
        assert_eq!(st["counts"]["manual"], json!(1));
        assert_eq!(st["saves"][0]["sequence"], json!(1));
        assert_eq!(
            reason(restore(app.world_mut(), &json!({ "slot": 9, "confirm": true }))),
            "persistence_slot_unknown"
        );
        let restored = restore(app.world_mut(), &json!({ "slot": 1, "confirm": true })).expect("restore");
        assert_eq!(restored["restored"], json!(true));
        assert_eq!(restored["sequence"], json!(1));
        // A slot whose file no longer decodes leaves the world alone.
        let path = st["saves"][0]["path"].as_str().expect("path").to_owned();
        std::fs::write(&path, b"FRTS").expect("corrupt");
        assert_eq!(
            reason(restore(app.world_mut(), &json!({ "slot": 1, "confirm": true }))),
            "persistence_save_corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
