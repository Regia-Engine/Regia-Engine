// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the dedicated-server host (spec 76 § 12,
//! `book/agents/net-dedicated-server.md`, ADR-1663): `server.status`, `server.kick`, `server.ban`,
//! `server.unban`, `server.bans`, `server.change_level`, `server.reload_config` and
//! `server.shutdown`. They run in the dedicated server's world: the host inserts a
//! `ServerHostLink` there when it starts.
//!
//! Errors are `F0002` with `data.reason` set to one of: `server_host_missing` (this process is
//! not a dedicated host), `net_not_server`, `player_unknown`, `ban_entry_unknown`,
//! `level_not_in_pak`, `config_invalid`, `server_draining`, `confirmation_required`.

use regia_app::prelude::*;
use regia_net::server_host::config::resolve_from_process;
use regia_net::server_host::{
    HostOpError, HostState, PlayerLayer, ServerHostLink, ban_link_peer, ban_player, kick_link_peer,
    kick_player, link_hosts_game_port, player_layer_in_world, unban_entry, unban_link_entry,
};
use regia_net::session::NetSession;
use serde_json::{Value, json};

use crate::net_session::f0002;
use crate::protocol::RpcError;

fn link(world: &World) -> Result<ServerHostLink, RpcError> {
    world.get_resource::<ServerHostLink>().cloned().ok_or_else(|| {
        f0002(
            "server_host_missing",
            "this process is not a dedicated server host",
            "run the game's server build (regia_net::server_host::ServerHost with an AppHostGame), with --agent",
        )
    })
}

fn server_session(world: &mut World) -> Result<Mut<'_, NetSession>, RpcError> {
    let s = world.get_resource_mut::<NetSession>().ok_or_else(|| {
        f0002(
            "net_not_server",
            "the app has no NetSession",
            "add regia_net::NetSessionPlugin",
        )
    })?;
    if s.server().is_none() {
        return Err(f0002(
            "net_not_server",
            "this process is not hosting",
            "host a session first",
        ));
    }
    Ok(s)
}

fn op_error(e: HostOpError) -> RpcError {
    let hint = match e.code {
        "player_unknown" => "server.status lists the players as peer.local",
        "player_ambiguous" => "say link:<id> (net.peers) or session:<id> (server.status)",
        "ban_entry_unknown" => "server.bans lists the entry ids",
        "ban_store_save_failed" => {
            "the entry is in memory for this run only; close whatever holds bans.ron and ban again"
        }
        _ => "see book/agents/net-dedicated-server.md",
    };
    f0002(e.code, e.message, hint)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` is required")))
}

/// `server.status`.
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let link = link(world)?;
    let shared = link.snapshot();
    let mut out: Value = serde_json::from_str(&shared.health_json).unwrap_or_else(|_| json!({}));
    let s = server_session(world)?;
    let server = s
        .server()
        .ok_or_else(|| f0002("net_not_server", "not hosting", "host a session first"))?;
    let mut players = Vec::new();
    for row in server.peer_rows() {
        for (id, name) in row.players.iter().zip(row.names.iter()) {
            players.push(json!({ "player": id, "name": name, "addr": row.addr, "state": row.state }));
        }
    }
    if let Value::Object(map) = &mut out {
        map.insert("players".into(), json!(players));
        map.insert("player_count".into(), json!(server.players_in_use()));
    }
    Ok(out)
}

/// `server.kick`.
pub fn kick(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    link(world)?;
    let player = str_param(params, "player")?.to_owned();
    let reason = crate::params::opt_str(params, "reason")?
        .unwrap_or("kicked by an admin")
        .to_owned();
    // A replication game's own port (ADR-2665 (4)): its players are link peers. Any other id may
    // be a session player (`--shared-port`), and an id on both layers has to say which.
    let mut target = player.as_str();
    if link_hosts_game_port(world) {
        let (layer, id) = player_layer_in_world(world, &player).map_err(op_error)?;
        if layer == PlayerLayer::Link {
            kick_link_peer(world, id, &reason).map_err(op_error)?;
            return Ok(json!({ "kicked": true }));
        }
        target = id;
    }
    let mut s = server_session(world)?;
    kick_player(&mut s, target, &reason).map_err(op_error)?;
    Ok(json!({ "kicked": true }))
}

/// `server.ban`.
pub fn ban(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    link(world)?;
    let player = str_param(params, "player")?.to_owned();
    let duration = crate::params::opt_u64(params, "duration_s")?;
    let reason = crate::params::opt_str(params, "reason")?
        .unwrap_or("banned by an admin")
        .to_owned();
    let mut target = player.as_str();
    if link_hosts_game_port(world) {
        let (layer, id) = player_layer_in_world(world, &player).map_err(op_error)?;
        if layer == PlayerLayer::Link {
            let entry = ban_link_peer(world, id, duration, &reason).map_err(op_error)?;
            return Ok(json!({ "entry_id": entry.0 }));
        }
        target = id;
    }
    let mut s = server_session(world)?;
    let id = ban_player(&mut s, target, duration, &reason).map_err(op_error)?;
    Ok(json!({ "entry_id": id.0 }))
}

/// `server.unban`.
pub fn unban(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    link(world)?;
    let id = params
        .get("entry_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entry_id` is required"))?;
    if link_hosts_game_port(world) {
        unban_link_entry(world, id).map_err(op_error)?;
        return Ok(json!({ "removed": true }));
    }
    let mut s = server_session(world)?;
    unban_entry(&mut s, id).map_err(op_error)?;
    Ok(json!({ "removed": true }))
}

/// `server.bans`.
pub fn bans(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    link(world)?;
    let s = server_session(world)?;
    let server = s
        .server()
        .ok_or_else(|| f0002("net_not_server", "not hosting", "host a session first"))?;
    let entries =
        serde_json::to_value(&server.bans().list().entries).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok(json!({ "entries": entries }))
}

/// `server.change_level`: travels to a level of the rotation now.
pub fn change_level(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let link = link(world)?;
    let level = str_param(params, "level")?.to_owned();
    let shared = link.snapshot();
    let in_rotation = shared.settings.rotation.iter().any(|row| row == &level)
        || shared.rotation.rows.iter().any(|row| row.map_id == level);
    if !in_rotation {
        return Err(f0002(
            "level_not_in_pak",
            format!("`{level}` is not a level of this server's rotation"),
            "use a levels.rotation entry (the console `level` command takes any level in the pak)",
        ));
    }
    if shared.state == HostState::Draining || shared.state == HostState::Stopped {
        return Err(f0002(
            "server_draining",
            "the server is shutting down",
            "wait for the restart",
        ));
    }
    let mut s = server_session(world)?;
    let travel_id = s
        .travel(&level)
        .map_err(|e| crate::net_session::session_error(&e))?;
    Ok(json!({ "travel_id": travel_id }))
}

/// `server.reload_config`: re-reads `server.toml` (the host applies it at its next tick).
pub fn reload_config(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let link = link(world)?;
    let shared = link.snapshot();
    let resolved = resolve_from_process(&shared.base_dir, &shared.cli).map_err(|errors| {
        let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
        f0002(
            "config_invalid",
            lines.join("; "),
            "fix server.toml; nothing was applied",
        )
    })?;
    let mut applied: Vec<String> = shared
        .settings
        .hot_changes(&resolved.settings)
        .into_iter()
        .map(str::to_owned)
        .collect();
    applied.push("bans.ron".into());
    let needs_restart: Vec<String> = shared
        .settings
        .restart_only_changes(&resolved.settings)
        .into_iter()
        .map(str::to_owned)
        .collect();
    link.signals.request_reload();
    Ok(json!({ "applied": applied, "needs_restart": needs_restart }))
}

fn no_rotation() -> RpcError {
    f0002(
        "server_no_rotation",
        "this process is not a dedicated host",
        "run a dedicated server (book/agents/net-session-players.md)",
    )
}

/// `server.rotation_get`: the live playlist rows, the current index, whether it loops, the order,
/// the rows the last travel skipped (ADR-8897), the last row that failed to load and the
/// playlist's rules hash (ADR-8881).
pub fn rotation_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let link = world
        .get_resource::<ServerHostLink>()
        .cloned()
        .ok_or_else(no_rotation)?;
    let view = link.snapshot().rotation;
    let rows: Vec<Value> = view
        .rows
        .iter()
        .map(|row| {
            let mut options = serde_json::Map::new();
            for (key, value) in &row.options {
                options.insert(key.clone(), Value::String(value.clone()));
            }
            json!({
                "map_id": row.map_id,
                "mode": row.mode,
                "options": options,
            })
        })
        .collect();
    let last_failure = view.last_failure.map(|failure| {
        json!({
            "map_id": failure.map_id,
            "mode": failure.mode,
            "error": failure.error,
        })
    });
    // ADR-8897: the order and the rows the last travel passed over, with their reason code.
    let skipped: Vec<Value> = view
        .skipped
        .iter()
        .map(|skip| {
            json!({
                "index": skip.index,
                "map_id": skip.map_id,
                "mode": skip.mode,
                "reason": skip.reason,
            })
        })
        .collect();
    Ok(json!({
        "rows": rows,
        "index": view.index,
        "looping": view.looping,
        "order": view.order,
        "skipped": skipped,
        "last_failure": last_failure,
        // ADR-8881: the playlist's combined rules hash (16 hex digits), or null when the game gave
        // the host no rule sets or no row names one; `rules_in_manifest` says whether the session
        // manifest carries it.
        "rules_hash": view.rules_hash.map(|hash| format!("{hash:016x}")),
        "rules_in_manifest": view.rules_in_manifest,
    }))
}

/// `server.rotation_skip`: asks the dedicated host to travel to the next row on its next tick.
pub fn rotation_skip(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let link = world
        .get_resource::<ServerHostLink>()
        .cloned()
        .ok_or_else(no_rotation)?;
    link.signals.request_rotation_skip();
    Ok(json!({ "skipped": true }))
}

/// `server.shutdown {grace_s?, confirm: true}`.
pub fn shutdown(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let link = link(world)?;
    if crate::params::opt_bool(params, "confirm")? != Some(true) {
        return Err(f0002(
            "confirmation_required",
            "server.shutdown stops the server for every player",
            "repeat the call with `confirm: true` once the owner agreed",
        ));
    }
    let grace = crate::params::opt_f64(params, "grace_s")?
        .unwrap_or(5.0)
        .clamp(0.0, 600.0);
    {
        let mut shared = link
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if shared.state == HostState::Draining || shared.state == HostState::Stopped {
            return Err(f0002(
                "server_draining",
                "the server is already shutting down",
                "wait for it to stop",
            ));
        }
        shared.drain_grace_s = Some(grace);
    }
    link.signals.request_shutdown();
    Ok(json!({ "draining": true }))
}

/// `server.lockstep_status` (ADR-8558): the lockstep match's rows, the `lockstep` object of the
/// host's `/health` body.
pub fn lockstep_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let link = link(world)?;
    let shared = link.snapshot();
    let health: Value = serde_json::from_str(&shared.health_json).unwrap_or_else(|_| json!({}));
    match health.get("lockstep") {
        Some(rows) if rows.is_object() => Ok(rows.clone()),
        _ => Err(f0002(
            "lockstep_not_running",
            "the dedicated server's game is not a lockstep server",
            "set [lockstep] enabled = true in server.toml and host the game's LockstepHostGame (book/agents/net-lockstep-server.md)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use regia_net::server_host::{HostShared, HostSignals, RotationView};
    use regia_net::session::hub::SimInput;
    use regia_net::session::lifecycle::HostOptions;

    fn reason(r: Result<Value, RpcError>) -> String {
        let e = r.expect_err("error");
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    #[test]
    fn rotation_methods_need_a_dedicated_host() {
        let _calls = [
            "server.rotation_get",
            "server.rotation_skip",
            "server_no_rotation",
        ];
        let mut world = World::new();
        assert_eq!(reason(rotation_get(&mut world, &json!({}))), "server_no_rotation");
        assert_eq!(
            reason(rotation_skip(&mut world, &json!({}))),
            "server_no_rotation"
        );
    }

    /// R-04 (5), ADR-8881: `server.rotation_get` shows the playlist's rules hash as 16 hex digits,
    /// and null when the host has none.
    #[test]
    fn rotation_get_shows_the_rules_hash() {
        let dir = temp_dir();
        let toml =
            "format_version = 1\n[server]\nport = 7791\n[levels]\nrotation = [\"levels/a.level.ron\"]\n";
        std::fs::write(dir.join("server.toml"), toml).expect("toml");
        let settings = resolve_from_process(&dir, &Default::default())
            .expect("config")
            .settings;
        let link = ServerHostLink {
            signals: HostSignals::new(),
            shared: Arc::new(Mutex::new(HostShared {
                health_json: "{}".into(),
                state: HostState::Running,
                settings,
                base_dir: dir.clone(),
                cli: Default::default(),
                drain_grace_s: None,
                rotation: RotationView::default(),
            })),
        };
        let mut world = World::new();
        world.insert_resource(link.clone());
        let view = rotation_get(&mut world, &json!({})).expect("view");
        assert_eq!(view["rules_hash"], Value::Null);
        link.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rotation
            .rules_hash = Some(0x00ab_cdef_0123_4567);
        let view = rotation_get(&mut world, &json!({})).expect("view");
        assert_eq!(view["rules_hash"], "00abcdef01234567");
        assert_eq!(view["rules_in_manifest"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-agent-server-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn every_method_round_trips_and_every_reason_is_produced() {
        let mut w = World::new();
        assert_eq!(reason(status(&mut w, &json!({}))), "server_host_missing");
        let dir = temp_dir();
        let toml = "format_version = 1\n[server]\nport = 7790\n[levels]\nrotation = [\"levels/a.level.ron\", \"levels/b.level.ron\"]\n";
        std::fs::write(dir.join("server.toml"), toml).expect("toml");
        let settings = resolve_from_process(&dir, &Default::default())
            .expect("config")
            .settings;
        let signals = HostSignals::new();
        let link = ServerHostLink {
            signals: signals.clone(),
            shared: Arc::new(Mutex::new(HostShared {
                health_json: "{\"ok\":true,\"state\":\"running\"}".into(),
                state: HostState::Running,
                settings: settings.clone(),
                base_dir: dir.clone(),
                cli: Default::default(),
                drain_grace_s: None,
                rotation: RotationView::default(),
            })),
        };
        w.insert_resource(link.clone());
        w.insert_resource(NetSession::default());
        assert_eq!(reason(status(&mut w, &json!({}))), "net_not_server");
        {
            let mut s = w.resource_mut::<NetSession>();
            s.set_manual_clock(0);
            s.host_local(
                HostOptions {
                    level: "levels/a.level.ron".into(),
                    ..HostOptions::default()
                },
                true,
            )
            .expect("host");
            s.add_simulated_clients(3, SimInput::Idle).expect("sims");
            for _ in 0..60 {
                s.advance_manual_clock(16);
                s.update();
            }
        }
        let st = status(&mut w, &json!({})).expect("status");
        assert_eq!(st["state"], "running");
        assert_eq!(st["players"].as_array().map(Vec::len), Some(3), "{st}");
        assert_eq!(st["players"][0]["player"], "1.0");
        assert_eq!(
            kick(&mut w, &json!({ "player": "1.0", "reason": "afk" })).expect("kick")["kicked"],
            true
        );
        assert_eq!(
            reason(kick(&mut w, &json!({ "player": "9.0" }))),
            "player_unknown"
        );
        let banned = ban(
            &mut w,
            &json!({ "player": "2", "duration_s": 600, "reason": "spam" }),
        )
        .expect("ban");
        assert_eq!(banned["entry_id"], 1);
        let list = bans(&mut w, &json!({})).expect("bans");
        assert_eq!(list["entries"][0]["kind"], "Address");
        assert_eq!(list["entries"][0]["reason"], "spam");
        assert_eq!(
            unban(&mut w, &json!({ "entry_id": 1 })).expect("unban")["removed"],
            true
        );
        assert_eq!(
            reason(unban(&mut w, &json!({ "entry_id": 1 }))),
            "ban_entry_unknown"
        );
        assert_eq!(
            reason(change_level(&mut w, &json!({ "level": "levels/zzz.level.ron" }))),
            "level_not_in_pak"
        );
        let t = change_level(&mut w, &json!({ "level": "levels/b.level.ron" })).expect("travel");
        assert!(t["travel_id"].as_u64().is_some());
        // Reload: a hot key and a restart key.
        std::fs::write(
            dir.join("server.toml"),
            toml.replace("port = 7790", "port = 7791\nmotd = \"hi\""),
        )
        .expect("toml");
        let r = reload_config(&mut w, &json!({})).expect("reload");
        assert!(
            r["applied"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("server.motd"))),
            "{r}"
        );
        assert_eq!(r["needs_restart"], json!(["server.port"]));
        assert!(signals.take_reload());
        std::fs::write(dir.join("server.toml"), "format_version = 9\n").expect("toml");
        assert_eq!(reason(reload_config(&mut w, &json!({}))), "config_invalid");
        // Shutdown needs confirmation, then drains once.
        assert_eq!(reason(shutdown(&mut w, &json!({}))), "confirmation_required");
        assert_eq!(
            shutdown(&mut w, &json!({ "grace_s": 2.0, "confirm": true })).expect("down")["draining"],
            true
        );
        assert!(signals.take_shutdown());
        assert_eq!(link.snapshot().drain_grace_s, Some(2.0));
        link.shared.lock().expect("lock").state = HostState::Draining;
        assert_eq!(
            reason(shutdown(&mut w, &json!({ "confirm": true }))),
            "server_draining"
        );
        assert_eq!(
            reason(change_level(&mut w, &json!({ "level": "levels/a.level.ron" }))),
            "server_draining"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-2665 (4): on a replication game's own port (`HostedLink`), `server.kick`, `server.ban`
    /// and `server.unban` act on the link's peers; the ban is an address entry of the session's
    /// list, listed by `server.bans`, and the link refuses the address until the unban.
    #[test]
    fn kick_and_ban_reach_peers_on_the_game_port() {
        use regia_net::bridge::NetFrame;
        use regia_net::link::{LinkClock, LinkEvent, LinkOptions, NetLink, NetLinkPlugin};
        use regia_net::model::NetMode;
        use regia_net::server_host::HostedLink;
        use regia_net::socket::LoopbackNet;
        use regia_net::{NetGame, NetPlugin};

        struct Wire;
        impl NetGame for Wire {
            type Message = NetFrame;
            type Input = u8;
            type JoinData = ();
            const GAME_ID: &'static str = "test.agent_link_bans";
            const PROTOCOL_VERSION: u32 = 1;
        }
        let opts = LinkOptions {
            clock: LinkClock::Manual {
                start_ms: 0,
                step_us: 16_000,
            },
            ..LinkOptions::default()
        };
        let app_for = |mode| {
            let mut app = App::new();
            regia_app::enable_fixed_step(&mut app, 60.0);
            app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
            app
        };
        let at = |s: &str| -> std::net::SocketAddr { s.parse().expect("addr") };
        let net = LoopbackNet::new();
        let dir = temp_dir();
        let mut sv = app_for(NetMode::DedicatedServer);
        sv.insert_resource(HostedLink::new::<Wire>(opts.clone()));
        sv.world_mut()
            .resource_mut::<NetLink>()
            .host::<Wire>(Box::new(net.bind(at("10.1.0.1:7000")).expect("bind")), &opts)
            .expect("host");
        let mut session = NetSession::default();
        session
            .host_local(
                HostOptions {
                    level: "levels/a.level.ron".into(),
                    ..HostOptions::default()
                },
                true,
            )
            .expect("host the session");
        sv.insert_resource(session);
        sv.insert_resource(ServerHostLink {
            signals: HostSignals::new(),
            shared: Arc::new(Mutex::new(HostShared {
                health_json: "{}".into(),
                state: HostState::Running,
                settings: Default::default(),
                base_dir: dir.clone(),
                cli: Default::default(),
                drain_grace_s: None,
                rotation: RotationView::default(),
            })),
        });
        let mut clients: Vec<App> = ["10.1.0.2:5000", "10.1.0.3:5000"]
            .iter()
            .map(|from| {
                let mut c = app_for(NetMode::Client);
                c.world_mut()
                    .resource_mut::<NetLink>()
                    .connect::<Wire>(
                        Box::new(net.bind(at(from)).expect("bind")),
                        at("10.1.0.1:7000"),
                        &opts,
                    )
                    .expect("connect");
                c
            })
            .collect();
        let pump = |sv: &mut App, clients: &mut [App], n: usize| {
            let mut reasons = vec![None; clients.len()];
            for _ in 0..n {
                sv.update();
                for (i, c) in clients.iter_mut().enumerate() {
                    c.update();
                    for e in c.world_mut().resource_mut::<NetLink>().take_events() {
                        if let LinkEvent::Ended { reason } = e {
                            reasons[i] = Some(reason);
                        }
                    }
                }
            }
            reasons
        };
        pump(&mut sv, &mut clients, 30);
        let peer_at = |sv: &App, from: &str| -> String {
            sv.world()
                .resource::<NetLink>()
                .peer_addrs()
                .into_iter()
                .find(|(_, a)| a.to_string() == from)
                .map(|(p, _)| p.0.to_string())
                .expect("joined")
        };
        let first = peer_at(&sv, "10.1.0.2:5000");
        let second = peer_at(&sv, "10.1.0.3:5000");
        let w = sv.world_mut();
        assert_eq!(
            kick(w, &json!({ "player": first, "reason": "afk" })).expect("kick")["kicked"],
            true
        );
        assert_eq!(reason(kick(w, &json!({ "player": "77" }))), "player_unknown");
        let banned = ban(
            w,
            &json!({ "player": second, "duration_s": 600, "reason": "spam" }),
        )
        .expect("ban");
        let entry = banned["entry_id"].as_u64().expect("entry id");
        let list = bans(w, &json!({})).expect("bans");
        assert_eq!(list["entries"][0]["kind"], "Address");
        assert_eq!(list["entries"][0]["value"], "10.1.0.3");
        let reasons = pump(&mut sv, &mut clients, 20);
        assert_eq!(reasons[0].as_deref(), Some("kicked"));
        assert_eq!(reasons[1].as_deref(), Some("banned"));
        assert!(sv.world().resource::<NetLink>().peer_addrs().is_empty());
        let w = sv.world_mut();
        assert_eq!(
            unban(w, &json!({ "entry_id": entry })).expect("unban")["removed"],
            true
        );
        assert_eq!(
            reason(unban(w, &json!({ "entry_id": entry }))),
            "ban_entry_unknown"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `server.lockstep_status` through the dispatcher: no host, a host whose game is not a
    /// lockstep server, and one that is (the rows of its `/health`).
    #[test]
    fn lockstep_status_reads_the_hosts_lockstep_rows() {
        let call = |w: &mut World| match crate::call_local(w, "server.lockstep_status", json!({}), None) {
            crate::dispatch::LocalReply::Done(r) => r,
            crate::dispatch::LocalReply::Pending(_) => panic!("answers at once"),
        };
        let mut w = World::new();
        assert_eq!(reason(call(&mut w)), "server_host_missing");
        let dir = temp_dir();
        let toml =
            "format_version = 4\n[levels]\nrotation = [\"levels/a.level.ron\"]\n[lockstep]\nenabled = true\n";
        std::fs::write(dir.join("server.toml"), toml).expect("toml");
        let settings = resolve_from_process(&dir, &Default::default())
            .expect("config")
            .settings;
        let shared = Arc::new(Mutex::new(HostShared {
            health_json: "{\"ok\":true,\"state\":\"running\"}".into(),
            state: HostState::Running,
            settings,
            base_dir: dir.clone(),
            cli: Default::default(),
            drain_grace_s: None,
            rotation: RotationView::default(),
        }));
        w.insert_resource(ServerHostLink {
            signals: HostSignals::new(),
            shared: Arc::clone(&shared),
        });
        assert_eq!(reason(call(&mut w)), "lockstep_not_running");
        shared.lock().expect("shared").health_json = "{\"ok\":true,\"lockstep\":{\"tick\":120,\"seats\":40,\"max_seats\":40,\"verdicts\":1,\"last_verdict\":{\"tick\":90,\"kind\":\"diverged\",\"agreeing\":[\"observer\",\"seat0\"],\"diverged\":[\"seat1\"],\"abstained\":[]},\"observer\":{\"voting\":true,\"lag_ticks\":1,\"dropouts\":0}}}".into();
        let rows = call(&mut w).expect("rows");
        assert_eq!(rows["seats"], 40);
        assert_eq!(rows["last_verdict"]["diverged"][0], "seat1");
        assert_eq!(rows["observer"]["voting"], true);
        let _ = std::fs::remove_dir_all(dir);
    }
}
