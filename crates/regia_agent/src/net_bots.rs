// SPDX-License-Identifier: MPL-2.0
//! Agent protocol for connectionless server bots (ADR-3346, `book/agents/net-session-players.md`).
//!
//! `net.bots` requires the play permission and a running server session. Errors are `F0002` with
//! `data.reason`: `net_not_running`, `net_not_server`, `bots_unavailable`, `bot_limit`,
//! `bot_unknown`, `player_slot_unavailable` or `invalid_value`.

use regia_app::prelude::*;
use regia_framework::net_bots::{
    BotDifficulty, NetBotError, NetBots, NetBotsConfig, NetSessionPlayerRow, add_bot, net_bot_rows,
    net_session_players, remove_bot, set_bot_difficulty,
};
use regia_net::session::NetSession;
use serde_json::{Value, json};

use crate::net_session::f0002;
use crate::protocol::RpcError;

fn server_ready(world: &World) -> Result<(), RpcError> {
    let Some(session) = world.get_resource::<NetSession>() else {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net session",
            "add NetSessionPlugin and host a server",
        ));
    };
    if session.server().is_none() {
        return Err(f0002(
            "net_not_server",
            "net.bots runs on a listen or dedicated server",
            "host a session first",
        ));
    }
    if !world.contains_resource::<NetBots>() || !world.contains_resource::<NetBotsConfig>() {
        return Err(f0002(
            "bots_unavailable",
            "the server-bot policy is not installed",
            "add NetBotsPlugin and ServerBotAiPlugin",
        ));
    }
    Ok(())
}

fn bot_error(error: NetBotError) -> RpcError {
    f0002(
        error.code(),
        error.to_string(),
        "see net.bots in the networking agent guide",
    )
}

fn invalid(message: impl Into<String>) -> RpcError {
    f0002(
        "invalid_value",
        message,
        "see net.bots in the networking agent guide",
    )
}

pub(crate) fn player_values(rows: Vec<NetSessionPlayerRow>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            json!({
                "player": row.player,
                "entity": row.entity.to_bits(),
                "pawn": row.pawn.map(Entity::to_bits),
                "is_bot": row.is_bot,
                "takeover_of": row.takeover_of,
            })
        })
        .collect()
}

fn rows_value(world: &mut World) -> Value {
    let bots: Vec<Value> = net_bot_rows(world)
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "is_bot": row.is_bot,
                "player": row.player.to_bits(),
                "pawn": row.pawn.map(Entity::to_bits),
                "takeover_of": row.takeover_of.map(|id| format!("{}.{}", id.peer.0, id.local)),
                "difficulty": {
                    "reaction_ticks": row.difficulty.reaction_ticks,
                    "aim_error_radians": row.difficulty.aim_error_radians,
                    "perception_radius": row.difficulty.perception_radius,
                },
            })
        })
        .collect();
    let players = player_values(net_session_players(world));
    json!({ "bots": bots, "players": players })
}

fn id_param(params: &Value) -> Result<u32, RpcError> {
    let id = crate::params::opt_u64(params, "id")?
        .ok_or_else(|| invalid("`id` must be a bot id in 0..=u32::MAX"))?;
    u32::try_from(id).map_err(|_| invalid("`id` must be a bot id in 0..=u32::MAX"))
}

fn difficulty_param(params: &Value, mut base: BotDifficulty) -> Result<BotDifficulty, RpcError> {
    if let Some(value) = crate::params::opt_u64(params, "reaction_ticks")? {
        base.reaction_ticks =
            u32::try_from(value).map_err(|_| invalid("`reaction_ticks` must be in 0..=u32::MAX"))?;
    }
    for (name, output, limit) in [
        (
            "aim_error_radians",
            &mut base.aim_error_radians,
            std::f32::consts::PI,
        ),
        ("perception_radius", &mut base.perception_radius, f32::MAX),
    ] {
        if let Some(value) = crate::params::opt_f64(params, name)? {
            if !value.is_finite() || value < 0.0 || value > f64::from(limit) {
                return Err(invalid(format!("`{name}` must be finite and in 0..={limit}")));
            }
            *output = value as f32;
        }
    }
    Ok(base)
}

/// `net.bots`: list, add, remove or change difficulty on the running server.
pub(crate) fn bots(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server_ready(world)?;
    match crate::params::opt_str(params, "action")?.unwrap_or("list") {
        "list" => Ok(rows_value(world)),
        "add" => {
            let base = world.resource::<NetBotsConfig>().difficulty;
            let difficulty = difficulty_param(params, base)?;
            let id = add_bot(world, difficulty).map_err(bot_error)?;
            let mut result = rows_value(world);
            result["added"] = json!(id);
            Ok(result)
        }
        "remove" => {
            let id = id_param(params)?;
            remove_bot(world, id).map_err(bot_error)?;
            let mut result = rows_value(world);
            result["removed"] = json!(id);
            Ok(result)
        }
        "set_difficulty" => {
            let id = id_param(params)?;
            let player = world
                .resource::<NetBots>()
                .player_entity(id)
                .ok_or_else(|| bot_error(NetBotError::Unknown))?;
            let base = world
                .get::<regia_framework::net_bots::NetBot>(player)
                .map_or_else(BotDifficulty::default, |bot| bot.difficulty);
            let difficulty = difficulty_param(params, base)?;
            set_bot_difficulty(world, id, difficulty).map_err(bot_error)?;
            Ok(rows_value(world))
        }
        _ => Err(invalid("`action` must be list, add, remove or set_difficulty")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::player::PlayerManager;
    use regia_framework::rules::GameRulesResource;
    use regia_net::NetMode;
    use regia_net::server_host::ServerSettings;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(NetSession::default());
        crate::net_session::host(&mut world, &json!({ "mode": "dedicated", "port": 0 }))
            .expect("loopback server");
        world.insert_resource(NetMode::DedicatedServer);
        world.insert_resource(PlayerManager::default());
        world.insert_resource(NetBots::default());
        world.insert_resource(NetBotsConfig {
            max_bots: 1,
            ..NetBotsConfig::default()
        });
        world.insert_resource(GameRulesResource::default());
        world.insert_resource(ServerSettings::default());
        world
    }

    fn reason(result: Result<Value, RpcError>) -> String {
        result
            .expect_err("error")
            .data
            .and_then(|data| data["reason"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }

    #[test]
    fn every_action_round_trips_and_errors_are_stable() {
        let mut bare = World::new();
        assert_eq!(reason(bots(&mut bare, &json!({}))), "net_not_running");
        let mut world = world();
        assert_eq!(
            reason(bots(&mut world, &json!({ "action": "nope" }))),
            "invalid_value"
        );
        assert_eq!(bots(&mut world, &json!({})).expect("list")["bots"], json!([]));
        let added = bots(&mut world, &json!({ "action": "add", "reaction_ticks": 0 })).expect("add");
        let id = added["added"].as_u64().expect("id") as u32;
        assert_eq!(added["bots"][0]["is_bot"], true);
        assert_eq!(added["players"][0]["is_bot"], true);
        assert_eq!(reason(bots(&mut world, &json!({ "action": "add" }))), "bot_limit");
        let changed = bots(
            &mut world,
            &json!({ "action": "set_difficulty", "id": id, "perception_radius": 12.5 }),
        )
        .expect("difficulty");
        assert_eq!(changed["bots"][0]["difficulty"]["perception_radius"], 12.5);
        assert_eq!(
            reason(bots(&mut world, &json!({ "action": "remove", "id": 999 }))),
            "bot_unknown"
        );
        bots(&mut world, &json!({ "action": "remove", "id": id })).expect("remove");
        assert_eq!(bots(&mut world, &json!({})).expect("list")["bots"], json!([]));
    }

    /// Spec 22 T2.3 addendum: `session.state` exposes `is_bot` on its player rows, as
    /// `net_session_players` does (ADR-3346; test added by ADR-5628).
    #[test]
    fn session_state_marks_bot_players() {
        let mut world = world();
        let before = crate::net_session::state(&mut world, &json!({})).expect("state");
        assert_eq!(before["role"], "server");
        assert_eq!(before["players"], json!([]));
        let added = bots(&mut world, &json!({ "action": "add", "reaction_ticks": 0 })).expect("add");
        let id = added["added"].clone();
        let state = crate::net_session::state(&mut world, &json!({})).expect("state");
        let rows = state["players"].as_array().expect("player rows");
        assert_eq!(rows.len(), 1, "{state}");
        assert_eq!(rows[0]["is_bot"], true, "{state}");
        assert!(
            rows[0]["player"].as_str().is_some_and(|p| p.starts_with("bot.")),
            "{state}"
        );
        bots(&mut world, &json!({ "action": "remove", "id": id })).expect("remove");
        let after = crate::net_session::state(&mut world, &json!({})).expect("state");
        assert_eq!(after["players"], json!([]));
    }
}
