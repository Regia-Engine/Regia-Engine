// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the session lifecycle (spec 77 § 14, `book/agents/net-session.md`,
//! ADR-1663): `session.state`, `session.host`, `session.connect`, `session.disconnect`,
//! `session.travel`, `session.manifest`, `net.conditioner_get`, `net.conditioner_apply` and
//! `net.sim_clients`; and (ADR-2413, `book/agents/net-session-players.md`) `net.player_stats`,
//! `net.stats_overlay` and `net.handoffs`.
//!
//! Handler errors are `F0002` with `data.reason` set to one of: `net_not_running` (no `NetSession`
//! resource), `already_in_session`, `net_not_server`, `level_not_found`, `join_code_unsupported`,
//! `transport_error`, `password_needs_encryption`, `conditioner_unavailable` (shipping build),
//! `preset_unknown`,
//! `sim_clients_unavailable` (shipping build), `sim_clients_limit`, `recording_invalid`. A bad
//! parameter (`session.host` `mode`, `port`, `address`) is `F0002` with `data.hint` and no
//! `data.reason`. The § 7.1 disconnect codes appear in `session.state`'s `reason`.

use std::net::SocketAddr;

use regia_app::prelude::*;
use regia_net::crypto::{CryptoMode, ServerKey};
use regia_net::session::NetSession;
use regia_net::session::conditioner::{
    SESSION_CONDITIONER_PRESETS, SessionConditioner, ShaperSettings, conditioner_available,
};
use regia_net::session::hub::{SimInput, SimRecording};
use regia_net::session::lifecycle::{ClientSessionState, HostOptions, SessionSettings};
use regia_net::session::net_session::{ConnectOptions, ConnectTarget, SessionError};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

pub(crate) fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn not_running() -> RpcError {
    f0002(
        "net_not_running",
        "the app has no regia_net session",
        "add regia_net::NetSessionPlugin (it inserts an idle NetSession)",
    )
}

pub(crate) fn session_error(e: &SessionError) -> RpcError {
    let hint = match e {
        SessionError::AlreadyInSession => "session.disconnect first, then host or connect again",
        SessionError::NotServer => "call it on the server or the listen host",
        SessionError::NotClient => "call it on a client",
        SessionError::LevelNotFound => "pass a level path the server has",
        SessionError::SimClientsLimit => "at most 256 simulated clients per session",
        SessionError::PasswordNeedsEncryption => "use encryption auto or required, or a loopback address",
        _ => "see book/agents/net-session.md",
    };
    f0002(e.code(), e.to_string(), hint)
}

fn session(world: &World) -> Result<&NetSession, RpcError> {
    world.get_resource::<NetSession>().ok_or_else(not_running)
}

fn session_mut(world: &mut World) -> Result<Mut<'_, NetSession>, RpcError> {
    world.get_resource_mut::<NetSession>().ok_or_else(not_running)
}

fn state_value(state: &ClientSessionState) -> Value {
    match state {
        ClientSessionState::Disconnected(Some(r)) => {
            json!({ "state": state.as_str(), "reason": r.code(), "detail": r.detail() })
        }
        _ => json!({ "state": state.as_str() }),
    }
}

/// `session.state`.
pub fn state(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let players = if world
        .get_resource::<NetSession>()
        .is_some_and(|session| session.server().is_some())
    {
        crate::net_bots::player_values(regia_framework::net_bots::net_session_players(world))
    } else {
        Vec::new()
    };
    let s = session(world)?;
    // The mods this process presents, with their version text (spec 77 § 4.1, ADR-7259).
    let mods: Vec<Value> = s
        .session_manifest()
        .mods
        .iter()
        .map(|m| json!({ "id": m.id, "version": m.version }))
        .collect();
    if let Some(server) = s.server() {
        let peers: Vec<Value> = server
            .peer_rows()
            .into_iter()
            .map(|r| json!({ "peer": r.peer, "players": r.players, "names": r.names, "state": r.state, "addr": r.addr, "interrupted": r.interrupted, "backpressured": server.is_backpressured(regia_net::PeerId(r.peer)) }))
            .collect();
        return Ok(json!({
            "role": "server",
            "session_id": server.session_id(),
            "level": server.level(),
            "travel_id": server.travel_id(),
            "dedicated": s.is_dedicated(),
            "peers": peers,
            "players": players,
            "encryption": s.settings().encryption.as_str(),
            "server_key": s.server_key().map(|k| k.to_string()),
            "join_snapshot": s.join_counts(),
            "travel": {
                "despawned": s.travel_counts().despawned,
                "persistent": s.travel_counts().persistent,
                "persistent_ids": s.persistent_ids(),
            },
            "mods": mods,
        }));
    }
    if let Some(client) = s.client() {
        let mut out = state_value(client.state());
        if let Value::Object(map) = &mut out {
            map.insert("role".into(), json!("client"));
            map.insert("interrupted".into(), json!(client.interrupted()));
            map.insert("server".into(), json!(s.server_addr().map(|a| a.to_string())));
            map.insert("peer_id".into(), json!(client.peer_id().map(|p| p.0)));
            map.insert("level".into(), json!(client.level()));
            map.insert("travel_id".into(), json!(client.travel_id()));
            map.insert("encryption".into(), json!(s.settings().encryption.as_str()));
            map.insert("sealed".into(), json!(s.link_sealed()));
            map.insert("server_key".into(), json!(s.server_key().map(|k| k.to_string())));
            map.insert("join_snapshot".into(), json!(s.join_counts()));
            let travel = s.client_travel_counts();
            // The counter is cumulative over the connection and counts every tombstone drop
            // (despawn and out-of-scope notices too), not only a travel's old-level records;
            // the schema text says so.
            let tombstoned = world
                .get_resource::<regia_net::replication::ClientReplication>()
                .map_or(0, |c| c.state.counters.tombstoned);
            map.insert(
                "travel".into(),
                json!({
                    "despawned": travel.despawned,
                    "persistent": travel.persistent,
                    "tombstoned": tombstoned,
                }),
            );
            map.insert("mods".into(), json!(mods));
        }
        return Ok(out);
    }
    Ok(json!({ "role": "idle", "state": "disconnected", "mods": mods }))
}

fn encryption_param(params: &Value) -> Result<Option<CryptoMode>, RpcError> {
    match params.get("encryption") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .and_then(CryptoMode::parse)
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`encryption` must be off, auto or required")),
    }
}

fn apply_encryption(session: &mut NetSession, mode: Option<CryptoMode>) {
    if let Some(encryption) = mode {
        session.set_settings(SessionSettings {
            encryption,
            ..session.settings().clone()
        });
    }
}

/// `session.host`.
pub fn host(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dedicated = match crate::params::opt_str(params, "mode")? {
        Some("dedicated") => true,
        Some("listen") => false,
        _ => {
            return Err(RpcError::invalid_params(
                "`mode` must be \"listen\" or \"dedicated\"",
            ));
        }
    };
    let port = match params.get("port") {
        None | Some(Value::Null) => 7777,
        Some(v) => v
            .as_u64()
            .filter(|p| *p <= 65_535)
            .ok_or_else(|| RpcError::invalid_params("`port` must be 0..=65535 (0 = any free port)"))?,
    };
    let mut opts = HostOptions::default();
    if let Some(level) = crate::params::opt_str(params, "level")? {
        opts.level = level.to_owned();
    }
    if let Some(n) = crate::params::opt_u64(params, "max_players")? {
        opts.max_players = n.clamp(1, 256) as u32;
    }
    if let Some(pw) = crate::params::opt_str(params, "password")? {
        opts.password = pw.to_owned();
    }
    let encryption = encryption_param(params)?;
    let mut s = session_mut(world)?;
    apply_encryption(&mut s, encryption);
    let bind = SocketAddr::from(([0, 0, 0, 0], port as u16));
    let id = s.host(bind, opts, dedicated).map_err(|e| session_error(&e))?;
    Ok(json!({ "session_id": id, "port": s.local_addr().map(|a| a.port()) }))
}

/// `session.connect`.
pub fn connect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = if let Some(addr) = crate::params::opt_str(params, "address")? {
        let a: SocketAddr = addr
            .parse()
            .map_err(|_| RpcError::invalid_params("`address` must be ip:port"))?;
        ConnectTarget::Address(a)
    } else if let Some(code) = crate::params::opt_str(params, "join_code")? {
        return Err(f0002(
            "join_code_unsupported",
            format!(
                "join code {code}: join codes resolve through the relay, which the session does not drive yet"
            ),
            "resolve the code through the relay and pass `address`",
        ));
    } else {
        return Err(RpcError::invalid_params("`address` or `join_code` is required"));
    };
    let mut opts = ConnectOptions::default();
    if let Some(list) = crate::params::opt_array(params, "players")? {
        opts.player_names = list
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
    }
    if let Some(pw) = crate::params::opt_str(params, "password")? {
        opts.password = Some(pw.to_owned());
    }
    if let Some(key) = params.get("server_key").filter(|v| !v.is_null()) {
        opts.server_key = Some(
            key.as_str()
                .and_then(ServerKey::parse)
                .ok_or_else(|| RpcError::invalid_params("`server_key` must be 64 hex digits"))?,
        );
    }
    // Trust on first use is on unless the caller opts out for LAN development (ADR-3283).
    opts.allow_unpinned = crate::params::opt_bool(params, "allow_unpinned")?.unwrap_or(false);
    let encryption = encryption_param(params)?;
    let mut s = session_mut(world)?;
    apply_encryption(&mut s, encryption);
    s.connect(target, opts).map_err(|e| session_error(&e))?;
    Ok(json!({ "state": s.state().as_str() }))
}

/// `session.disconnect`.
pub fn disconnect(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut s = session_mut(world)?;
    s.disconnect();
    let state = match s.role_name() {
        "server" => "draining",
        "client" => s.state().as_str(),
        _ => "disconnected",
    };
    Ok(json!({ "state": state }))
}

/// `session.travel`.
pub fn travel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let level = params
        .get("level")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`level` is required"))?
        .to_owned();
    let mut s = session_mut(world)?;
    if s.server().is_none() {
        return Err(session_error(&SessionError::NotServer));
    }
    let travel_id = s.travel(&level).map_err(|e| session_error(&e))?;
    Ok(json!({ "travel_id": travel_id }))
}

/// `session.manifest`: what this process presents at session start, mounted mods included (spec 77
/// § 4.1, ADR-7212).
pub fn manifest(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let s = session(world)?;
    serde_json::to_value(s.session_manifest()).map_err(|e| RpcError::internal(e.to_string()))
}

/// `net.conditioner_get`.
pub fn conditioner_get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let s = session(world)?;
    let c = s.conditioner();
    Ok(json!({
        "outbound": c.outbound,
        "inbound": c.inbound,
        "seed": c.seed,
        "presets": SESSION_CONDITIONER_PRESETS,
        "available": conditioner_available(),
    }))
}

fn shaper_param(params: &Value, key: &str, base: ShaperSettings) -> Result<ShaperSettings, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(base),
        Some(v) => serde_json::from_value::<ShaperSettings>(v.clone())
            .map(ShaperSettings::sanitized)
            .map_err(|e| RpcError::invalid_params(format!("`{key}`: {e}"))),
    }
}

/// `net.conditioner_apply`.
pub fn conditioner_apply(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !conditioner_available() {
        return Err(f0002(
            "conditioner_unavailable",
            "the network conditioner is compiled out of shipping builds",
            "use a dev build",
        ));
    }
    let mut s = session_mut(world)?;
    let mut c = *s.conditioner();
    if let Some(name) = crate::params::opt_str(params, "preset")? {
        let seed = c.seed;
        c = SessionConditioner::preset(name).ok_or_else(|| {
            f0002(
                "preset_unknown",
                format!("unknown conditioner preset `{name}`"),
                "use off, lan, good, average, bad or terrible",
            )
        })?;
        c.seed = seed;
    }
    c.outbound = shaper_param(params, "outbound", c.outbound)?;
    c.inbound = shaper_param(params, "inbound", c.inbound)?;
    if let Some(seed) = crate::params::opt_u64(params, "seed")? {
        c.seed = seed;
    }
    s.set_conditioner(c);
    Ok(json!({ "outbound": c.outbound, "inbound": c.inbound, "seed": c.seed }))
}

/// `net.sim_clients`.
pub fn sim_clients(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !conditioner_available() {
        return Err(f0002(
            "sim_clients_unavailable",
            "simulated clients are a dev-build tool",
            "use a dev build",
        ));
    }
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`action` is required: add, remove or clear"))?;
    let count = crate::params::opt_u64(params, "count")?.unwrap_or(1) as usize;
    // A recording is read through the one document-path rule (ADR-7388): the project or the temp
    // dir only, capped, and never a FIFO that would block the world thread. A wrong-typed
    // `recording` is `invalid_params` (ADR-2622; ADR-7570, ADR-7566), not an absent one.
    let recording = match crate::params::opt_str(params, "recording")? {
        Some(path) if action == "add" => {
            Some((path, crate::req_path::document_path(world, "recording", path)?))
        }
        _ => None,
    };
    let mut s = session_mut(world)?;
    if s.server().is_none() {
        return Err(session_error(&SessionError::NotServer));
    }
    let live = match action {
        "add" => {
            let seed = crate::params::opt_u64(params, "seed")?.unwrap_or(1);
            let input = match crate::params::opt_str(params, "input")?.unwrap_or("idle") {
                "idle" => SimInput::Idle,
                "random" => SimInput::Random { seed },
                "recording" => {
                    let (path, file) = recording.as_ref().ok_or_else(|| {
                        RpcError::invalid_params("`recording` (a *.siminput.ron path) is required")
                    })?;
                    let text = crate::req_path::read_file_capped(file, crate::req_path::MAX_READ_BYTES)
                        .and_then(|bytes| String::from_utf8(bytes).map_err(std::io::Error::other))
                        .map_err(|e| {
                            f0002(
                                "recording_invalid",
                                format!("{path}: {e}"),
                                "pass a readable *.siminput.ron file",
                            )
                        })?;
                    let rec = SimRecording::from_ron(&text).map_err(|e| {
                        f0002(
                            "recording_invalid",
                            format!("{path}: {e}"),
                            "see docs/formats/siminput-ron.md",
                        )
                    })?;
                    SimInput::Recording(rec.frames)
                }
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "`input` must be idle, random or recording, not `{other}`"
                    )));
                }
            };
            s.add_simulated_clients(count, input)
                .map_err(|e| session_error(&e))?
        }
        "remove" => s.remove_simulated_clients(count).map_err(|e| session_error(&e))?,
        "clear" => s.clear_simulated_clients().map_err(|e| session_error(&e))?,
        other => {
            return Err(RpcError::invalid_params(format!(
                "`action` must be add, remove or clear, not `{other}`"
            )));
        }
    };
    Ok(json!({ "simulated": live }))
}

/// The `net.stats` result for a session: one row per loopback or simulated client plus the
/// overlay text lines (spec 77 § 10.2). Shared with the editor's play-in-editor methods.
#[must_use]
pub fn stats_value(session: &NetSession) -> Value {
    let rows = session.connection_stats();
    let lines: Vec<String> = rows.iter().map(|r| r.line()).collect();
    json!({
        "role": session.role_name(),
        "now_ms": session.now_ms(),
        "connections": rows,
        "lines": lines,
    })
}

/// The `net.traffic` result for a session: bytes per message kind (spec 77 § 10.2).
#[must_use]
pub fn traffic_value(session: &NetSession) -> Value {
    let view = session.bandwidth_view();
    json!({
        "kinds": view.kinds,
        "total_out": view.total_out,
        "total_in": view.total_in,
        "lines": view.lines(),
    })
}

/// `net.stats`: the stats overlay rows (server sessions; a client or idle session has none).
pub fn stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(stats_value(session(world)?))
}

/// `net.traffic`: the bandwidth view, bytes per message kind of the session's links, plus the
/// per-type payload bytes of the replication layer when the app registers net messages.
pub fn traffic(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut out = traffic_value(session(world)?);
    let types: Vec<Value> = world
        .get_resource::<regia_net::message::NetMessageRegistry>()
        .map(|reg| {
            reg.types()
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let c = reg.counters.get(i).copied().unwrap_or_default();
                    json!({ "type": t.name, "bytes_sent": c.bytes_sent, "bytes_received": c.bytes_received })
                })
                .collect()
        })
        .unwrap_or_default();
    if let Value::Object(map) = &mut out {
        map.insert("messages".into(), Value::Array(types));
    }
    Ok(out)
}

fn link_value(s: &regia_net::session::PlayerLinkStats) -> Value {
    json!({
        "ping_ms": s.ping_ms,
        "jitter_ms": s.jitter_ms,
        "loss_to_player": s.loss_to_player,
        "loss_from_player": s.loss_from_player,
        "simulated": s.simulated,
    })
}

/// `net.player_stats` (ADR-2413): ping and loss per remote player (server) or of the link to the
/// server (client), read live from the transport's estimates.
pub fn player_stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let session = session(world)?;
    let (rows, server) = session.player_link_stats();
    let players: Vec<Value> = rows
        .iter()
        .map(|(p, s)| {
            let mut v = link_value(s);
            if let Value::Object(map) = &mut v {
                map.insert("player".into(), json!(format!("{}.{}", p.peer.0, p.local)));
                map.insert("peer".into(), json!(p.peer.0));
                map.insert("local".into(), json!(p.local));
                map.insert(
                    "state".into(),
                    json!(session.peer_state(p.peer).map(|st| st.as_str())),
                );
            }
            v
        })
        .collect();
    let refreshed = world
        .get_resource::<regia_net::session::NetPlayerStats>()
        .and_then(|s| s.refreshed_ms);
    Ok(json!({
        "role": session.role_name(),
        "now_ms": session.now_ms(),
        "refreshed_ms": refreshed,
        "players": players,
        "server": server.as_ref().map(link_value),
    }))
}

/// `net.stats_overlay` (ADR-2413): shows or hides the in-game stats overlay (`visible`), or flips it
/// without the parameter; returns the state and the overlay's text lines.
pub fn stats_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    session(world)?;
    let want = match params.get("visible") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => {
            return Err(f0002(
                "invalid_value",
                "`visible` must be a boolean",
                "pass { \"visible\": true } or no parameters to toggle",
            ));
        }
    };
    let mut overlay = world.get_resource_or_insert_with(regia_net::session::NetStatsOverlay::default);
    match want {
        Some(v) => overlay.visible = v,
        None => {
            overlay.toggle();
        }
    }
    let visible = overlay.visible;
    Ok(json!({
        "visible": visible,
        "lines": regia_net::session::players::overlay_lines(world),
    }))
}

/// `net.handoffs` (ADR-2413): the most recent hand-offs of entities whose owner left (newest last).
pub fn handoffs(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    session(world)?;
    let (total, records) = world
        .get_resource::<regia_net::session::NetHandOffs>()
        .map(|h| {
            let rows: Vec<Value> = h
                .records
                .iter()
                .map(|r| {
                    let peer = match r.to {
                        regia_net::session::OwnedHandOff::Peer(p) => Some(p.0),
                        _ => None,
                    };
                    json!({ "entity": r.entity, "from": r.from.0, "to": r.to.as_str(), "peer": peer })
                })
                .collect();
            (h.total, rows)
        })
        .unwrap_or_default();
    Ok(json!({ "total": total, "records": records }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::session::lifecycle::ServerSessionEvent;

    fn reason(r: Result<Value, RpcError>) -> String {
        let e = r.expect_err("error");
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    fn world() -> World {
        let mut w = World::new();
        let mut s = NetSession::default();
        s.set_manual_clock(0);
        w.insert_resource(s);
        w
    }

    fn pump(worlds: &mut [&mut World], steps: usize) {
        for _ in 0..steps {
            for w in worlds.iter_mut() {
                let mut s = w.resource_mut::<NetSession>();
                s.advance_manual_clock(16);
                s.update();
            }
        }
    }

    /// Spec 77 §§ 6.1-6.2 (ADR-7212) through the dispatcher: `session.state` reports the join
    /// snapshots a server sent and what its last travel despawned and kept, with the persistent
    /// `NetId`s.
    #[test]
    fn session_state_reports_join_snapshots_and_travel_through_the_dispatcher() {
        use regia_net::model::NetId;
        use regia_net::session::{HostOptions, SimInput};
        use regia_net::{NetPersistAcrossTravel, Replicated};

        let mut w = world();
        w.init_resource::<regia_app::Time>();
        {
            let mut s = w.resource_mut::<NetSession>();
            s.host_local(
                HostOptions {
                    level: "levels/a.level.ron".into(),
                    ..HostOptions::default()
                },
                true,
            )
            .expect("host");
            s.add_simulated_clients(1, SimInput::Idle).expect("sim");
        }
        let step = |w: &mut World, n: usize| {
            for _ in 0..n {
                w.resource_mut::<NetSession>().advance_manual_clock(16);
                regia_net::session::net_session::net_session_system(w);
            }
        };
        step(&mut w, 30);
        w.spawn((Replicated::default(), NetPersistAcrossTravel, NetId(7)));
        w.spawn((Replicated::default(), NetId(8)));
        let travel = crate::dispatch::call_in_process(
            &mut w,
            "session.travel",
            json!({ "level": "levels/b.level.ron" }),
        )
        .expect("not deferred")
        .expect("travel");
        assert!(travel["travel_id"].as_u64().is_some());
        step(&mut w, 30);
        let st = crate::dispatch::call_in_process(&mut w, "session.state", json!({}))
            .expect("not deferred")
            .expect("state");
        assert_eq!(st["role"], "server");
        assert_eq!(st["travel"]["despawned"], 1, "{st}");
        assert_eq!(st["travel"]["persistent"], 1, "{st}");
        assert_eq!(st["travel"]["persistent_ids"], json!([7]), "{st}");
        // The join and the rejoin after the travel each sent a snapshot.
        assert_eq!(st["join_snapshot"]["snapshots"], 2, "{st}");
    }

    /// ADR-7259: `session.state` lists the mounted mods with the version text of their own
    /// `luamod.ron` (empty for a mod that declares none), as `session.manifest` does.
    #[test]
    fn session_state_lists_mod_versions_through_the_dispatcher() {
        use regia_asset::mods_vfs::{ModSource, ModsVfs};
        use regia_net::session::HostOptions;

        let mut w = world();
        w.init_resource::<regia_app::Time>();
        let mut vfs = ModsVfs::new();
        let files = |version: Option<&str>| {
            let mut map = std::collections::BTreeMap::new();
            map.insert("levels/a.level.ron".to_owned(), b"()".to_vec());
            if let Some(v) = version {
                map.insert(
                    "luamod.ron".to_owned(),
                    format!("(id: \"x\", version: \"{v}\")").into_bytes(),
                );
            }
            ModSource::Memory(map)
        };
        vfs.mount("speedrun", files(Some("2.1"))).expect("mount");
        vfs.mount("skins", files(None)).expect("mount");
        w.insert_resource(vfs);
        regia_net::session::net_session::net_session_system(&mut w);
        let idle = crate::dispatch::call_in_process(&mut w, "session.state", json!({}))
            .expect("not deferred")
            .expect("state");
        let want = json!([{ "id": "skins", "version": "" }, { "id": "speedrun", "version": "2.1" }]);
        assert_eq!(idle["mods"], want, "{idle}");
        w.resource_mut::<NetSession>()
            .host_local(HostOptions::default(), true)
            .expect("host");
        let st = crate::dispatch::call_in_process(&mut w, "session.state", json!({}))
            .expect("not deferred")
            .expect("state");
        assert_eq!(st["role"], "server");
        assert_eq!(st["mods"], want, "{st}");
        let manifest = crate::dispatch::call_in_process(&mut w, "session.manifest", json!({}))
            .expect("not deferred")
            .expect("manifest");
        assert_eq!(manifest["mods"][1]["version"], "2.1", "{manifest}");
    }

    #[test]
    fn session_state_reports_interruption_without_changing_phase() {
        let net = regia_net::LoopbackNet::new();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 7777));
        let mut server = world();
        let mut client = world();
        server
            .resource_mut::<NetSession>()
            .host_on(
                net.bind(addr).expect("server socket"),
                HostOptions::default(),
                true,
            )
            .expect("host");
        client
            .resource_mut::<NetSession>()
            .connect_on(
                net.bind(std::net::SocketAddr::from(([127, 0, 0, 1], 8000)))
                    .expect("client socket"),
                addr,
                ConnectOptions::default(),
            )
            .expect("connect");
        pump(&mut [&mut client, &mut server], 200);
        let clean = state(&mut server, &json!({})).expect("clean");
        assert_eq!(clean["peers"][0]["state"], "in_game");
        assert_eq!(clean["peers"][0]["interrupted"], false);
        // Flow control (spec 120 § 10.3): an idle link is not backed up.
        assert_eq!(clean["peers"][0]["backpressured"], false);
        // Stop the client's transport pump for 3.2 s of the shared virtual clock.
        pump(&mut [&mut server], 200);
        let interrupted = state(&mut server, &json!({})).expect("interrupted");
        assert_eq!(interrupted["peers"][0]["state"], "in_game");
        assert_eq!(interrupted["peers"][0]["interrupted"], true);
        // A silent peer gets no new stream chunks: it reads as backed up.
        assert_eq!(interrupted["peers"][0]["backpressured"], true);
        let now_ms = server.resource::<NetSession>().now_ms();
        client.resource_mut::<NetSession>().set_manual_clock(now_ms);
        pump(&mut [&mut client, &mut server], 200);
        let restored = state(&mut server, &json!({})).expect("restored");
        assert_eq!(restored["peers"][0]["state"], "in_game");
        assert_eq!(restored["peers"][0]["interrupted"], false);
        assert_eq!(restored["peers"][0]["peer"], clean["peers"][0]["peer"]);
    }

    #[test]
    fn every_method_round_trips_and_every_reason_is_produced() {
        // No session resource.
        let mut bare = World::new();
        assert_eq!(reason(state(&mut bare, &json!({}))), "net_not_running");

        // Idle.
        let mut server = world();
        assert_eq!(state(&mut server, &json!({})).expect("idle")["role"], "idle");
        assert_eq!(
            reason(travel(&mut server, &json!({ "level": "a" }))),
            "net_not_server"
        );
        assert_eq!(
            reason(sim_clients(&mut server, &json!({ "action": "add" }))),
            "net_not_server"
        );
        assert_eq!(
            disconnect(&mut server, &json!({})).expect("idle")["state"],
            "disconnected"
        );

        // Host on an OS-chosen port.
        let hosted = host(
            &mut server,
            &json!({ "mode": "dedicated", "port": 0, "level": "levels/a.level.ron" }),
        )
        .expect("host");
        let port = hosted["port"].as_u64().expect("port");
        assert!(port > 0);
        assert_eq!(
            reason(host(&mut server, &json!({ "mode": "listen", "port": 0 }))),
            "already_in_session"
        );
        assert_eq!(
            manifest(&mut server, &json!({})).expect("manifest")["format_version"],
            1
        );
        // Conditioner.
        let got = conditioner_get(&mut server, &json!({})).expect("get");
        assert_eq!(got["presets"].as_array().map(Vec::len), Some(6));
        let applied = conditioner_apply(&mut server, &json!({ "preset": "lan", "seed": 9 })).expect("apply");
        assert_eq!(applied["outbound"]["latency_ms"], 1);
        assert_eq!(applied["seed"], 9);
        assert_eq!(
            reason(conditioner_apply(&mut server, &json!({ "preset": "nope" }))),
            "preset_unknown"
        );
        // Simulated clients.
        assert_eq!(
            sim_clients(
                &mut server,
                &json!({ "action": "add", "count": 2, "input": "random", "seed": 3 })
            )
            .expect("add")["simulated"],
            2
        );
        assert_eq!(
            reason(sim_clients(
                &mut server,
                &json!({ "action": "add", "count": 300 })
            )),
            "sim_clients_limit"
        );
        assert_eq!(
            reason(sim_clients(
                &mut server,
                &json!({ "action": "add", "input": "recording", "recording": "nonexistent-x.siminput.ron" })
            )),
            "recording_invalid"
        );
        // ADR-7388 (lane v05-rc1-paths): a recording path is a document path; one outside the
        // project and the temp dir is refused before anything is read.
        for outside in [
            "../x.siminput.ron",
            "a/../../x.siminput.ron",
            "C:\\x.siminput.ron",
            "/etc/hostname",
        ] {
            assert_eq!(
                reason(sim_clients(
                    &mut server,
                    &json!({ "action": "add", "input": "recording", "recording": outside })
                )),
                "path_outside_project",
                "{outside}"
            );
        }
        // A real client over UDP on loopback.
        let mut client = world();
        assert_eq!(
            reason(connect(&mut client, &json!({ "join_code": "ABCD-EFGH" }))),
            "join_code_unsupported"
        );
        let addr = format!("127.0.0.1:{port}");
        assert_eq!(
            connect(&mut client, &json!({ "address": addr, "players": ["Ann"] })).expect("connect")["state"],
            "connecting"
        );
        assert_eq!(
            reason(connect(&mut client, &json!({ "address": addr }))),
            "already_in_session"
        );
        let mut joined = false;
        for _ in 0..400 {
            pump(&mut [&mut client, &mut server], 1);
            if state(&mut client, &json!({})).expect("state")["state"] == "in_game" {
                joined = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(joined, "{:?}", state(&mut client, &json!({})));
        let st = state(&mut server, &json!({})).expect("server state");
        assert_eq!(st["role"], "server");
        assert!(st["peers"].as_array().is_some_and(|p| p.len() >= 3), "{st}");
        // ADR-7295: the client branch of `session.state` carries the tombstoned-records counter
        // (what a travel's tombstones dropped), read from the world's replication state.
        assert_eq!(
            state(&mut client, &json!({})).expect("client state")["travel"]["tombstoned"],
            0,
            "no replication client: no tombstoned records"
        );
        let mut replication = regia_net::replication::ClientReplication::default();
        replication.state.counters.tombstoned = 5;
        client.insert_resource(replication);
        assert_eq!(
            state(&mut client, &json!({})).expect("client state")["travel"]["tombstoned"],
            5,
            "the counter maps into session.state"
        );
        client.remove_resource::<regia_net::replication::ClientReplication>();
        assert_eq!(
            reason(travel(&mut client, &json!({ "level": "x" }))),
            "net_not_server"
        );
        assert_eq!(
            reason(travel(&mut server, &json!({ "level": " " }))),
            "level_not_found"
        );
        let t = travel(&mut server, &json!({ "level": "levels/b.level.ron" })).expect("travel");
        assert!(t["travel_id"].as_u64().is_some());
        assert_eq!(
            sim_clients(&mut server, &json!({ "action": "remove", "count": 1 })).expect("remove")["simulated"],
            1
        );
        assert_eq!(
            sim_clients(&mut server, &json!({ "action": "clear" })).expect("clear")["simulated"],
            0
        );
        assert_eq!(
            disconnect(&mut client, &json!({})).expect("bye")["state"],
            "disconnecting"
        );
        pump(&mut [&mut client, &mut server], 10);
        let st = state(&mut client, &json!({})).expect("state");
        assert_eq!(st["reason"], "client_quit", "{st}");
        assert_eq!(
            disconnect(&mut server, &json!({})).expect("drain")["state"],
            "draining"
        );
        let events = server.resource_mut::<NetSession>().take_server_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ServerSessionEvent::PlayerJoined { .. }))
        );
        // transport_error: a port that cannot be bound.
        let mut other = world();
        let taken = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind");
        let p = taken.local_addr().expect("addr").port();
        assert_eq!(
            reason(host(&mut other, &json!({ "mode": "listen", "port": p }))),
            "transport_error"
        );
    }

    /// ADR-2622: a `recording` of the wrong JSON type is `invalid_params` naming the parameter, not
    /// read as "no recording" (which answered `recording` is required, or ignored it on `remove`).
    #[test]
    fn a_non_string_recording_is_invalid_params() {
        let mut server = world();
        host(&mut server, &json!({ "mode": "dedicated", "port": 0 })).expect("host");
        for action in ["add", "remove"] {
            for bad in [
                json!(7),
                json!(true),
                json!(["a.siminput.ron"]),
                json!({ "path": "a" }),
            ] {
                let err = sim_clients(
                    &mut server,
                    &json!({ "action": action, "input": "recording", "recording": bad }),
                )
                .expect_err("a wrong-typed recording is refused");
                assert_eq!(err.code, INVALID_PARAMS, "{action} {bad}: {err:?}");
                let data = err.data.as_ref().expect("data");
                assert_eq!(data["param"], "recording", "{action} {bad}: {err:?}");
                assert_eq!(data["expected"], "a string", "{action} {bad}: {err:?}");
            }
        }
        // Nothing was added by the refused calls.
        assert_eq!(
            sim_clients(&mut server, &json!({ "action": "remove", "count": 0 })).expect("remove")["simulated"],
            0
        );
    }

    #[test]
    fn encryption_and_server_key_params() {
        let mut w = world();
        let listen = json!({ "mode": "listen", "port": 0, "encryption": "maybe" });
        assert!(host(&mut w, &listen).is_err(), "a bad mode binds nothing");
        assert_eq!(w.resource::<NetSession>().role_name(), "idle");
        let connect_to = |extra: Value| {
            let mut params = json!({ "address": "127.0.0.1:9" });
            if let (Value::Object(p), Value::Object(e)) = (&mut params, extra) {
                p.extend(e);
            }
            params
        };
        assert!(connect(&mut w, &connect_to(json!({ "encryption": 1 }))).is_err());
        assert!(connect(&mut w, &connect_to(json!({ "server_key": "abc" }))).is_err());
        assert_eq!(w.resource::<NetSession>().role_name(), "idle");

        // A password on the host's public bind with encryption off.
        let plain = json!({ "mode": "listen", "port": 0, "password": "pw", "encryption": "off" });
        assert_eq!(reason(host(&mut w, &plain)), "password_needs_encryption");
        // And a client's, to a non-loopback address.
        let remote = json!({ "address": "10.0.0.1:7777", "password": "pw", "encryption": "off" });
        assert_eq!(reason(connect(&mut w, &remote)), "password_needs_encryption");

        // A pinned client seals even on loopback, and both sides report the key.
        let mut server = world();
        let hosted = host(&mut server, &json!({ "mode": "listen", "port": 0 })).expect("host");
        let port = hosted["port"].as_u64().expect("port");
        let key = state(&mut server, &json!({})).expect("state")["server_key"]
            .as_str()
            .expect("the server reports its key")
            .to_owned();
        let mut client = world();
        let address = format!("127.0.0.1:{port}");
        connect(&mut client, &json!({ "address": address, "server_key": key })).expect("connect");
        pump(&mut [&mut client, &mut server], 400);
        let seen = state(&mut client, &json!({})).expect("state");
        assert_eq!(seen["sealed"], json!(true), "{seen}");
        assert_eq!(seen["server_key"], json!(key));
        assert_eq!(seen["encryption"], json!("auto"));
    }
    #[test]
    fn player_stats_overlay_and_handoffs() {
        let mut bare = World::new();
        assert_eq!(reason(player_stats(&mut bare, &json!({}))), "net_not_running");
        assert_eq!(reason(stats_overlay(&mut bare, &json!({}))), "net_not_running");
        assert_eq!(reason(handoffs(&mut bare, &json!({}))), "net_not_running");

        let mut server = world();
        server
            .resource_mut::<NetSession>()
            .host_local(HostOptions::default(), false)
            .expect("host");
        sim_clients(&mut server, &json!({ "action": "add", "count": 2 })).expect("add");
        // The session system spawns the player entities the rows follow.
        for _ in 0..200 {
            server.resource_mut::<NetSession>().advance_manual_clock(16);
            regia_net::session::net_session::net_session_system(&mut server);
        }
        let st = player_stats(&mut server, &json!({})).expect("stats");
        assert_eq!(st["role"], "server");
        let rows = st["players"].as_array().expect("rows");
        assert_eq!(rows.len(), 2, "{st}");
        assert_eq!(rows[0]["player"], "1.0");
        assert_eq!(rows[0]["state"], "in_game");
        assert_eq!(rows[0]["simulated"], true);
        assert!(st["refreshed_ms"].as_u64().is_some(), "{st}");

        // The overlay switch: off, toggled on, set off.
        let on = stats_overlay(&mut server, &json!({})).expect("toggle");
        assert_eq!(on["visible"], true);
        assert!(
            on["lines"][0].as_str().unwrap_or("").starts_with("net: server"),
            "{on}"
        );
        let off = stats_overlay(&mut server, &json!({ "visible": false })).expect("set");
        assert_eq!(off["visible"], false);
        assert_eq!(
            reason(stats_overlay(&mut server, &json!({ "visible": 1 }))),
            "invalid_value"
        );
        let h = handoffs(&mut server, &json!({})).expect("handoffs");
        assert_eq!(h["total"], 0);
    }

    #[test]
    fn stats_and_traffic_read_the_meters_of_the_clients() {
        let mut bare = World::new();
        assert_eq!(reason(stats(&mut bare, &json!({}))), "net_not_running");
        assert_eq!(reason(traffic(&mut bare, &json!({}))), "net_not_running");

        // An idle session has no rows.
        let mut idle = world();
        assert_eq!(
            stats(&mut idle, &json!({})).expect("idle")["connections"],
            json!([])
        );
        assert_eq!(traffic(&mut idle, &json!({})).expect("idle")["total_in"], 0);

        let mut server = world();
        {
            let mut s = server.resource_mut::<NetSession>();
            s.host_local(HostOptions::default(), false).expect("host");
        }
        sim_clients(
            &mut server,
            &json!({ "action": "add", "count": 2, "input": "random", "seed": 4 }),
        )
        .expect("add");
        pump(&mut [&mut server], 200);
        let st = stats(&mut server, &json!({})).expect("stats");
        let rows = st["connections"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["label"], "sim-1");
        assert_eq!(rows[0]["state"], "in_game");
        assert_eq!(rows[0]["simulated"], true);
        assert!(rows[0]["bytes_out"].as_u64().expect("out") > 0);
        assert_eq!(st["lines"].as_array().map(Vec::len), Some(2));
        let tr = traffic(&mut server, &json!({})).expect("traffic");
        let kinds: Vec<&str> = tr["kinds"]
            .as_array()
            .expect("kinds")
            .iter()
            .filter_map(|k| k["kind"].as_str())
            .collect();
        assert!(
            kinds.contains(&"hello") && kinds.contains(&"welcome"),
            "{kinds:?}"
        );
        // Random simulated inputs travel as game messages from the clients.
        assert!(kinds.contains(&"game"), "{kinds:?}");
        let out: u64 = tr["kinds"]
            .as_array()
            .expect("kinds")
            .iter()
            .map(|k| k["bytes_out"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(out, tr["total_out"].as_u64().expect("total"));
        assert_eq!(
            tr["messages"],
            json!([]),
            "no net messages registered in this world"
        );
    }
}
