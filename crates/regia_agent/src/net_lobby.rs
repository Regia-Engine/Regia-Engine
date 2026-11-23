// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of lobbies and matchmaking (spec 89 § 9, `book/agents/net-lobbies.md`,
//! ADR-1877): `lobby.status`, `lobby.list`, `lobby.create`, `lobby.join`, `lobby.leave`,
//! `lobby.set_data`, `lobby.ready`, `lobby.chat`, `lobby.launch`, `matchmaking.start`,
//! `matchmaking.cancel` and `matchmaking.status`.
//!
//! Lobby and member ids are 64-bit (Steam ids): they are written as decimal strings and read from
//! a string or an integer. Errors are `F0002` with `data.reason`: `lobby_not_running` (no
//! `Lobbies` resource), `matchmaking_busy`, `no_transport`, `session_error`, `net_not_running`
//! (launch without a `NetSession`) and the `LobbyError` codes (`lobby_unavailable`,
//! `not_in_lobby`, `already_in_lobby`, `not_owner`, `lobby_full`, `lobby_not_found`,
//! `key_invalid`, `value_too_long`, `reserved_key`, `too_many_keys`, `chat_invalid`,
//! `members_invalid`, `query_invalid`, `lobby_platform`, `lobby_no_identity`).

use std::collections::BTreeMap;
use std::net::SocketAddr;

use regia_app::prelude::*;
use regia_net::lobby::{
    Compare, Distance, LaunchError, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyQuery, LobbyServer,
    LobbySpec, LobbyVisibility, MatchState, NearFilter, NumberFilter, StringFilter,
};
use regia_net::session::NetSession;
use regia_net::session::lifecycle::HostOptions;
use serde_json::{Value, json};

use crate::net_session::f0002;
use crate::protocol::RpcError;

fn not_running() -> RpcError {
    f0002(
        "lobby_not_running",
        "the app has no regia_net Lobbies resource",
        "add regia_net::LobbyPlugin (or regia_steam::SteamLobbyPlugin for Steam lobbies)",
    )
}

fn lobby_error(e: &LobbyError) -> RpcError {
    let hint = match e {
        LobbyError::Unavailable => "Steam is not running or the feature is off; use the local backend in dev",
        LobbyError::NotInLobby => "lobby.create, lobby.join or matchmaking.start first",
        LobbyError::AlreadyInLobby => "lobby.leave first",
        LobbyError::NotOwner => "only the lobby owner may do this; lobby.status shows the owner",
        LobbyError::Full => "pick another lobby (lobby.list)",
        LobbyError::ReservedKey => "keys starting `regia.` belong to the engine",
        LobbyError::NoIdentity => {
            "add NetSessionPlugin, or have the game call Lobbies::set_identity or Lobbies::set_game; lobby.status shows identity"
        }
        _ => "see book/agents/net-lobbies.md",
    };
    f0002(e.code(), e.to_string(), hint)
}

fn launch_error(e: &LaunchError) -> RpcError {
    match e {
        LaunchError::Lobby(l) => lobby_error(l),
        LaunchError::NoTransport => f0002(
            "no_transport",
            e.to_string(),
            "pass `address`, or install a lobby transport (SteamLobbyPlugin)",
        ),
        LaunchError::Session(_) => f0002("session_error", e.to_string(), "session.state shows the session"),
    }
}

fn lobbies(world: &World) -> Result<&regia_net::Lobbies, RpcError> {
    world.get_resource::<regia_net::Lobbies>().ok_or_else(not_running)
}

fn lobbies_mut(world: &mut World) -> Result<Mut<'_, regia_net::Lobbies>, RpcError> {
    // A create, list or matchmaking call may come before the first pump: the engine keys follow
    // the session manifest from the first call on (spec 89 § 4, ADR-7213).
    regia_net::lobby::lobby_follow_manifest(world);
    world
        .get_resource_mut::<regia_net::Lobbies>()
        .ok_or_else(not_running)
}

fn id_param(params: &Value, name: &str) -> Result<u64, RpcError> {
    match params.get(name) {
        Some(Value::String(s)) => s.parse().ok(),
        Some(v) => v.as_u64(),
        None => None,
    }
    .filter(|id| *id != 0)
    .ok_or_else(|| {
        RpcError::invalid_params(format!(
            "`{name}` must be a non-zero lobby id (string or integer)"
        ))
    })
}

fn server_value(server: &LobbyServer) -> Value {
    json!(server.to_key())
}

fn info_value(info: &LobbyInfo) -> Value {
    let members: Vec<Value> = info
        .members
        .iter()
        .map(|m| json!({ "id": m.id.0.to_string(), "name": m.name, "data": m.data }))
        .collect();
    json!({
        "id": info.id.0.to_string(),
        "owner": info.owner.0.to_string(),
        "visibility": info.visibility.name(),
        "max_members": info.max_members,
        "member_count": info.member_count,
        "joinable": info.joinable,
        "members": members,
        "data": info.data,
        "server": info.server.as_ref().map(server_value),
    })
}

fn event_value(e: &LobbyEvent) -> Value {
    let mut v = json!({ "kind": e.kind() });
    if let Some(lobby) = e.lobby() {
        v["lobby"] = json!(lobby.0.to_string());
    }
    match e {
        LobbyEvent::Created { result: Err(err), .. }
        | LobbyEvent::Entered { result: Err(err), .. }
        | LobbyEvent::List { result: Err(err), .. } => v["error"] = json!(err.code()),
        LobbyEvent::List { result: Ok(list), .. } => v["count"] = json!(list.len()),
        LobbyEvent::MemberJoined { member, .. } | LobbyEvent::MemberLeft { member, .. } => {
            v["member"] = json!(member.0.to_string());
        }
        LobbyEvent::DataChanged { member, .. } => {
            v["member"] = json!(member.map(|m| m.0.to_string()));
        }
        LobbyEvent::OwnerChanged { owner, .. } => v["owner"] = json!(owner.0.to_string()),
        LobbyEvent::Chat { from, text, .. } => {
            v["from"] = json!(from.0.to_string());
            v["text"] = json!(text);
        }
        LobbyEvent::ServerSet { server, .. } => v["server"] = server_value(server),
        LobbyEvent::InviteAccepted { from, .. } => v["from"] = json!(from.0.to_string()),
        LobbyEvent::Left { reason, .. } => v["reason"] = json!(reason.code()),
        LobbyEvent::Reached { member, path, .. } => {
            v["member"] = json!(member.0.to_string());
            v["path"] = json!(path.as_str());
        }
        _ => {}
    }
    v
}

fn match_value(l: &regia_net::Lobbies) -> Value {
    let mm = l.matchmaker();
    let state = mm.state();
    let mut v = json!({
        "state": state.name(),
        "round": mm.round(),
        "candidates": mm.candidates().iter().map(|c| c.0.to_string()).collect::<Vec<_>>(),
        "lobby": state.lobby().map(|id| id.0.to_string()),
        "ready_to_launch": l.ready_to_launch(),
    });
    match state {
        MatchState::Launched { server, .. } => v["server"] = server_value(server),
        MatchState::Failed(f) => v["error"] = json!(f.code()),
        _ => {}
    }
    v
}

/// `lobby.status`.
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let l = lobbies(world)?;
    let last_list = match l.last_list() {
        Some(Ok(list)) => json!(list.iter().map(info_value).collect::<Vec<_>>()),
        Some(Err(e)) => json!({ "error": e.code() }),
        None => Value::Null,
    };
    Ok(json!({
        "backend": l.backend().name(),
        "transport": l.transport_name(),
        "local_member": l.local_member().map(|m| m.0.to_string()),
        "lobby": l.snapshot().map(info_value),
        "owner": l.is_owner(),
        "connected_to": l.connected_to().as_ref().map(server_value),
        "connect_error": l.last_launch_error(),
        "matchmaking": match_value(l),
        "last_list": last_list,
        "recent_events": l.recent_events().map(event_value).collect::<Vec<_>>(),
        "identity": identity_value(l),
    }))
}

/// `lobby.status` `identity`: where the lobby identity came from and its values (ADR-7881), with
/// ADR-7213's `protocol_hash`, `content_hash` and `from_manifest` (one object since integ-v06p, ADR-8385).
fn identity_value(l: &regia_net::Lobbies) -> Value {
    let (game, protocol, content) = l.identity_values();
    json!({
        "source": l.identity().name(),
        "game": game,
        "protocol": regia_net::lobby::hash_hex(protocol),
        "content": regia_net::lobby::hash_hex(content),
        "protocol_hash": regia_net::lobby::hash_hex(protocol),
        "content_hash": regia_net::lobby::hash_hex(content),
        "from_manifest": l.identity() != regia_net::lobby::LobbyIdentity::Explicit,
        "allow_unidentified": l.unidentified_allowed(),
    })
}

fn compare(v: &Value) -> Result<Compare, RpcError> {
    let name = crate::params::opt_str(v, "op")?.unwrap_or("eq");
    Compare::from_name(name)
        .ok_or_else(|| RpcError::invalid_params(format!("unknown op `{name}`: eq, ne, lt, le, gt, ge")))
}

fn key_of(v: &Value) -> Result<String, RpcError> {
    crate::params::opt_str(v, "key")?
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params("every filter needs a string `key`"))
}

fn number_of(v: &Value) -> Result<i32, RpcError> {
    crate::params::opt_i64(v, "value")?
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| RpcError::invalid_params("number filters need an i32 `value`"))
}

fn query_param(params: &Value) -> Result<LobbyQuery, RpcError> {
    let mut q = LobbyQuery::default();
    let list = |name: &str| -> Result<Vec<Value>, RpcError> {
        Ok(crate::params::opt_array(params, name)?
            .cloned()
            .unwrap_or_default())
    };
    for f in list("strings")? {
        let value = f
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("string filters need a string `value`"))?
            .to_owned();
        q.strings.push(StringFilter {
            key: key_of(&f)?,
            value,
            op: compare(&f)?,
        });
    }
    for f in list("numbers")? {
        q.numbers.push(NumberFilter {
            key: key_of(&f)?,
            value: number_of(&f)?,
            op: compare(&f)?,
        });
    }
    for f in list("near")? {
        q.near.push(NearFilter {
            key: key_of(&f)?,
            value: number_of(&f)?,
        });
    }
    if let Some(n) = crate::params::opt_u64(params, "open_slots")? {
        q.open_slots = Some(n.min(250) as u32);
    }
    if let Some(d) = crate::params::opt_str(params, "distance")? {
        q.distance = Distance::from_name(d)
            .ok_or_else(|| RpcError::invalid_params("`distance` is close, default, far or worldwide"))?;
    }
    if let Some(n) = crate::params::opt_u64(params, "max_results")? {
        q.max_results = n.min(u64::from(u32::MAX)) as u32;
    }
    q.validate().map_err(|e| lobby_error(&e))?;
    Ok(q)
}

fn data_param(params: &Value) -> Result<BTreeMap<String, String>, RpcError> {
    let Some(obj) = params.get("data") else {
        return Ok(BTreeMap::new());
    };
    let obj = obj
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("`data` must be an object of string values"))?;
    obj.iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|s| (k.clone(), s.to_owned()))
                .ok_or_else(|| RpcError::invalid_params("`data` values must be strings"))
        })
        .collect()
}

/// `lobby.list`.
pub fn list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = query_param(params)?;
    let mut l = lobbies_mut(world)?;
    let request = l.list(query).map_err(|e| lobby_error(&e))?;
    Ok(json!({ "request": request.0 }))
}

fn spec_param(l: &regia_net::Lobbies, params: &Value) -> Result<LobbySpec, RpcError> {
    let mut spec = l.default_spec();
    if let Some(v) = crate::params::opt_str(params, "visibility")? {
        spec.visibility = LobbyVisibility::from_name(v).ok_or_else(|| {
            RpcError::invalid_params("`visibility` is public, friends_only, private or invisible")
        })?;
    }
    if let Some(n) = crate::params::opt_u64(params, "max_members")? {
        spec.max_members = n.min(u64::from(u32::MAX)) as u32;
    }
    spec.data = data_param(params)?;
    Ok(spec)
}

/// `lobby.create`.
pub fn create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut l = lobbies_mut(world)?;
    let spec = spec_param(&l, params)?;
    let request = l.create(spec).map_err(|e| lobby_error(&e))?;
    Ok(json!({ "request": request.0 }))
}

/// `lobby.join`.
pub fn join(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params, "lobby")?;
    let mut l = lobbies_mut(world)?;
    let request = l.join(LobbyId(id)).map_err(|e| lobby_error(&e))?;
    Ok(json!({ "request": request.0 }))
}

/// `lobby.leave`.
pub fn leave(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut l = lobbies_mut(world)?;
    let left = l.current().map(|id| id.0.to_string());
    l.leave();
    Ok(json!({ "left": left }))
}

/// `lobby.set_data`.
pub fn set_data(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`key` (string) is required"))?
        .to_owned();
    let value = match params.get("value") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`value` must be a string or null (delete)",
            ));
        }
    };
    let member = crate::params::opt_bool(params, "member")?.unwrap_or(false);
    let mut l = lobbies_mut(world)?;
    let r = if member {
        l.set_member_data(&key, value.as_deref())
    } else {
        l.set_data(&key, value.as_deref())
    };
    r.map_err(|e| lobby_error(&e))?;
    Ok(json!({ "ok": true }))
}

/// `lobby.ready`.
pub fn ready(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ready = params
        .get("ready")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`ready` (bool) is required"))?;
    let mut l = lobbies_mut(world)?;
    l.set_ready(ready).map_err(|e| lobby_error(&e))?;
    Ok(json!({ "ready": ready }))
}

/// `lobby.chat`.
pub fn chat(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`text` (string) is required"))?
        .to_owned();
    let mut l = lobbies_mut(world)?;
    l.send_chat(&text).map_err(|e| lobby_error(&e))?;
    Ok(json!({ "ok": true }))
}

/// `lobby.launch`.
pub fn launch(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(addr) = crate::params::opt_str(params, "address")? {
        let a: SocketAddr = addr
            .parse()
            .map_err(|_| RpcError::invalid_params("`address` must be ip:port"))?;
        let server = LobbyServer::Address(a);
        lobbies_mut(world)?.launch(server).map_err(|e| lobby_error(&e))?;
        return Ok(json!({ "server": server_value(&server) }));
    }
    lobbies(world)?;
    if !world.contains_resource::<NetSession>() {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net session to host",
            "add regia_net::NetSessionPlugin, or pass `address` of a session hosted elsewhere",
        ));
    }
    let level = crate::params::opt_str(params, "level")?.map(str::to_owned);
    let result = world.resource_scope(|world, mut l: Mut<regia_net::Lobbies>| {
        let mut session = world.resource_mut::<NetSession>();
        let mut opts = HostOptions::default();
        if let Some(level) = level {
            opts.level = level;
        }
        opts.max_players = l.settings().max_members.max(1);
        l.launch_hosting(&mut session, opts)
    });
    let server = result.map_err(|e| launch_error(&e))?;
    Ok(json!({ "server": server_value(&server) }))
}

/// `matchmaking.start`.
pub fn matchmaking_start(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = query_param(params)?;
    let mut l = lobbies_mut(world)?;
    if l.matchmaker().state().is_searching() {
        return Err(f0002(
            "matchmaking_busy",
            "matchmaking is already searching",
            "matchmaking.cancel first",
        ));
    }
    let mut req = l.default_request();
    req.query = query;
    if let Some(n) = crate::params::opt_u64(params, "min_players")? {
        req.min_players = n.clamp(1, 250) as u32;
    }
    if let Some(b) = crate::params::opt_bool(params, "create_if_none")? {
        req.create_if_none = b;
    }
    req.create = spec_param(&l, params)?;
    l.start_matchmaking(req).map_err(|e| lobby_error(&e))?;
    Ok(json!({ "state": l.matchmaker().state().name() }))
}

/// `matchmaking.cancel`.
pub fn matchmaking_cancel(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut l = lobbies_mut(world)?;
    l.cancel_matchmaking();
    Ok(json!({ "state": l.matchmaker().state().name() }))
}

/// `matchmaking.status`.
pub fn matchmaking_status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    Ok(match_value(lobbies(world)?))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regia_net::lobby::LoopbackLobbyTransport;
    use regia_net::{LobbyPlugin, LocalLobbyHub, LoopbackNet, NetSessionPlugin};

    use super::*;

    fn app(hub: &LocalLobbyHub, net: &LoopbackNet, id: u64) -> App {
        let mut app = App::new();
        let mut l = regia_net::Lobbies::new(Arc::new(hub.member(id, &format!("p{id}"))));
        l.set_transport(Arc::new(LoopbackLobbyTransport::new(net.clone())));
        app.insert_resource(l);
        let mut s = NetSession::default();
        s.set_manual_clock(0);
        app.insert_resource(s);
        app.add_plugins((NetSessionPlugin, LobbyPlugin));
        app
    }

    fn reason(e: RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    fn frames(apps: &mut [&mut App], n: usize) {
        for _ in 0..n {
            for app in apps.iter_mut() {
                app.world_mut()
                    .resource_mut::<NetSession>()
                    .advance_manual_clock(16);
                app.update();
            }
        }
    }

    #[test]
    fn every_method_end_to_end_on_the_local_backend() {
        let hub = LocalLobbyHub::new();
        let net = LoopbackNet::new();
        let mut a = app(&hub, &net, 1);
        let mut b = app(&hub, &net, 2);
        let w = a.world_mut();
        assert_eq!(
            reason(set_data(w, &json!({ "key": "map", "value": "x" })).expect_err("no lobby")),
            "not_in_lobby"
        );
        assert_eq!(
            reason(create(w, &json!({ "data": { "regia.state": "x" } })).expect_err("reserved")),
            "reserved_key"
        );
        create(
            w,
            &json!({ "visibility": "public", "max_members": 4, "data": { "mode": "duel" } }),
        )
        .expect("create");
        frames(&mut [&mut a], 1);
        let st = status(a.world_mut(), &json!({})).expect("status");
        assert_eq!(st["backend"], "local");
        assert_eq!(st["owner"], true);
        // Spec 89 § 4 (ADR-7213): no game identity, so the lobby carries the session manifest's hashes.
        let manifest = a.world().resource::<NetSession>().manifest().clone();
        assert_eq!(st["identity"]["from_manifest"], true);
        assert_eq!(
            st["identity"]["protocol_hash"],
            regia_net::lobby::hash_hex(manifest.protocol_hash)
        );
        assert_eq!(
            st["lobby"]["data"]["regia.content"],
            regia_net::lobby::hash_hex(manifest.content_hash)
        );
        let lobby_id = st["lobby"]["id"].as_str().expect("id").to_owned();
        assert_eq!(st["lobby"]["data"]["mode"], "duel");
        assert_eq!(st["recent_events"][0]["kind"], "created");

        list(
            b.world_mut(),
            &json!({ "strings": [{ "key": "mode", "op": "eq", "value": "duel" }], "open_slots": 1 }),
        )
        .expect("list");
        frames(&mut [&mut b], 1);
        let st = status(b.world_mut(), &json!({})).expect("status");
        assert_eq!(st["last_list"][0]["id"], lobby_id.as_str());
        assert_eq!(
            reason(list(b.world_mut(), &json!({ "max_results": 0 })).expect_err("bad query")),
            "query_invalid"
        );
        join(b.world_mut(), &json!({ "lobby": lobby_id })).expect("join");
        frames(&mut [&mut a, &mut b], 1);
        assert_eq!(
            reason(join(b.world_mut(), &json!({ "lobby": lobby_id })).expect_err("again")),
            "already_in_lobby"
        );
        assert_eq!(
            reason(set_data(b.world_mut(), &json!({ "key": "map", "value": "x" })).expect_err("not owner")),
            "not_owner"
        );
        set_data(
            b.world_mut(),
            &json!({ "key": "team", "value": "red", "member": true }),
        )
        .expect("member data");
        set_data(a.world_mut(), &json!({ "key": "map", "value": "harbor" })).expect("lobby data");
        chat(b.world_mut(), &json!({ "text": "hi" })).expect("chat");
        assert_eq!(
            reason(chat(b.world_mut(), &json!({ "text": "" })).expect_err("empty")),
            "chat_invalid"
        );
        ready(a.world_mut(), &json!({ "ready": true })).expect("ready");
        ready(b.world_mut(), &json!({ "ready": true })).expect("ready");
        frames(&mut [&mut a, &mut b], 1);
        let st = matchmaking_status(a.world_mut(), &json!({})).expect("status");
        assert_eq!(st["ready_to_launch"], true);
        assert_eq!(st["state"], "idle");
        let st = status(a.world_mut(), &json!({})).expect("status");
        assert_eq!(st["lobby"]["members"][1]["data"]["team"], "red");
        let kinds: Vec<&str> = st["recent_events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|e| e["kind"].as_str())
            .collect();
        assert!(
            kinds.contains(&"member_joined") && kinds.contains(&"chat"),
            "{kinds:?}"
        );

        assert_eq!(
            reason(launch(b.world_mut(), &json!({})).expect_err("not owner")),
            "not_owner"
        );
        let out = launch(a.world_mut(), &json!({ "level": "levels/arena.level.ron" })).expect("launch");
        assert!(
            out["server"]
                .as_str()
                .expect("server")
                .starts_with("udp:127.0.0.1:")
        );
        frames(&mut [&mut a, &mut b], 120);
        let st = status(b.world_mut(), &json!({})).expect("status");
        assert_eq!(st["connected_to"], out["server"]);
        assert_eq!(
            b.world().resource::<NetSession>().state(),
            regia_net::session::lifecycle::ClientSessionState::InGame
        );
        let left = leave(b.world_mut(), &json!({})).expect("leave");
        assert_eq!(left["left"], lobby_id.as_str());
    }

    #[test]
    fn matchmaking_methods_and_errors() {
        let hub = LocalLobbyHub::new();
        let net = LoopbackNet::new();
        let mut a = app(&hub, &net, 1);
        let st = matchmaking_start(
            a.world_mut(),
            &json!({ "strings": [{ "key": "mode", "value": "ffa" }], "create_if_none": true, "data": { "mode": "ffa" } }),
        )
        .expect("start");
        assert_eq!(st["state"], "listing");
        assert_eq!(
            reason(matchmaking_start(a.world_mut(), &json!({})).expect_err("busy")),
            "matchmaking_busy"
        );
        frames(&mut [&mut a], 6);
        let st = matchmaking_status(a.world_mut(), &json!({})).expect("status");
        assert_eq!(st["state"], "in_lobby");
        let mut b = app(&hub, &net, 2);
        matchmaking_start(
            b.world_mut(),
            &json!({ "strings": [{ "key": "mode", "value": "ffa" }] }),
        )
        .expect("start");
        frames(&mut [&mut a, &mut b], 3);
        let sa = matchmaking_status(a.world_mut(), &json!({})).expect("status");
        let sb = matchmaking_status(b.world_mut(), &json!({})).expect("status");
        assert_eq!(sb["state"], "in_lobby");
        assert_eq!(sa["lobby"], sb["lobby"]);
        let st = matchmaking_cancel(b.world_mut(), &json!({})).expect("cancel");
        assert_eq!(st["state"], "idle");
        assert_eq!(
            reason(launch(b.world_mut(), &json!({ "address": "10.0.0.1:7777" })).expect_err("left")),
            "not_in_lobby"
        );
        let out = launch(a.world_mut(), &json!({ "address": "10.0.0.1:7777" })).expect("launch by address");
        assert_eq!(out["server"], "udp:10.0.0.1:7777");

        // No resource, no transport.
        let mut bare = App::new();
        assert_eq!(
            reason(status(bare.world_mut(), &json!({})).expect_err("none")),
            "lobby_not_running"
        );
        let mut unidentified = regia_net::Lobbies::new(Arc::new(hub.member(3, "c")));
        // No LobbyPlugin to wire the identity: opt in to an empty one (ADR-7881).
        unidentified.allow_unidentified(true);
        bare.insert_resource(unidentified);
        bare.insert_resource(NetSession::default());
        create(bare.world_mut(), &json!({})).expect("create");
        bare.world_mut().resource_mut::<regia_net::Lobbies>().pump();
        assert_eq!(
            reason(launch(bare.world_mut(), &json!({})).expect_err("no transport")),
            "no_transport"
        );
        let unavailable = regia_net::Lobbies::default();
        bare.insert_resource(unavailable);
        assert_eq!(
            reason(matchmaking_start(bare.world_mut(), &json!({})).expect_err("unavailable")),
            "lobby_unavailable"
        );
    }

    /// Spec 89 § 9: the create fields of `lobby.create` and `matchmaking.start` answer the
    /// documented reasons when the call is made, and nothing is left running.
    #[test]
    fn create_fields_answer_their_codes_up_front() {
        let hub = LocalLobbyHub::new();
        let net = LoopbackNet::new();
        let mut a = app(&hub, &net, 1);
        let long = "v".repeat(regia_net::lobby::MAX_VALUE_BYTES + 1);
        let too_many: serde_json::Map<String, Value> = (0..regia_net::lobby::MAX_KEYS)
            .map(|i| (format!("k{i}"), json!("v")))
            .collect();
        let cases = [
            (json!({ "max_members": 0 }), "members_invalid"),
            (json!({ "max_members": 251 }), "members_invalid"),
            (json!({ "data": { "": "x" } }), "key_invalid"),
            (json!({ "data": { "mode": long } }), "value_too_long"),
            (json!({ "data": too_many }), "too_many_keys"),
        ];
        for (params, code) in &cases {
            assert_eq!(
                reason(create(a.world_mut(), params).expect_err("create")),
                *code,
                "lobby.create {params}"
            );
            assert_eq!(
                reason(matchmaking_start(a.world_mut(), params).expect_err("start")),
                *code,
                "matchmaking.start {params}"
            );
            let st = matchmaking_status(a.world_mut(), &json!({})).expect("status");
            assert_eq!(st["state"], "idle", "{params}");
        }
        frames(&mut [&mut a], 2);
        assert!(hub.lobbies().is_empty(), "no lobby was created");
    }

    /// Spec 89 § 5: the create fields are checked up front only when `create_if_none`: a
    /// join-only `matchmaking.start` never sends them, so `max_members: 0` or too many keys
    /// still starts, lists and joins an open lobby.
    #[test]
    fn join_only_matchmaking_ignores_the_unused_create_fields() {
        let hub = LocalLobbyHub::new();
        let net = LoopbackNet::new();
        let mut owner = app(&hub, &net, 1);
        create(owner.world_mut(), &json!({ "max_members": 8 })).expect("create");
        frames(&mut [&mut owner], 1);
        let lobby = status(owner.world_mut(), &json!({})).expect("status")["lobby"]["id"].clone();
        let too_many: serde_json::Map<String, Value> = (0..regia_net::lobby::MAX_KEYS)
            .map(|i| (format!("k{i}"), json!("v")))
            .collect();
        let cases = [
            (json!({ "max_members": 0 }), "members_invalid"),
            (json!({ "max_members": 251 }), "members_invalid"),
            (json!({ "data": too_many }), "too_many_keys"),
        ];
        for (i, (fields, code)) in cases.iter().enumerate() {
            let mut joiner = app(&hub, &net, 10 + i as u64);
            let mut params = fields.clone();
            params["create_if_none"] = json!(true);
            assert_eq!(
                reason(matchmaking_start(joiner.world_mut(), &params).expect_err("create")),
                *code,
                "matchmaking.start {params}"
            );
            params["create_if_none"] = json!(false);
            let st = matchmaking_start(joiner.world_mut(), &params)
                .unwrap_or_else(|e| panic!("matchmaking.start {params}: {e:?}"));
            assert_eq!(st["state"], "listing", "{params}");
            frames(&mut [&mut owner, &mut joiner], 4);
            let st = matchmaking_status(joiner.world_mut(), &json!({})).expect("status");
            assert_eq!(st["state"], "in_lobby", "{params}");
            assert_eq!(st["lobby"], lobby, "{params}");
            matchmaking_cancel(joiner.world_mut(), &json!({})).expect("cancel");
            frames(&mut [&mut owner, &mut joiner], 1);
        }
    }

    /// Spec 89 § 9: `lobby.launch` without an address in an app with no `NetSession` answers
    /// `net_not_running`.
    #[test]
    fn launch_without_a_session_answers_net_not_running() {
        let hub = LocalLobbyHub::new();
        let mut app = App::new();
        let mut lobbies = regia_net::Lobbies::new(Arc::new(hub.member(1, "p1")));
        // No LobbyPlugin or NetSession to take an identity from: opt in (ADR-7881).
        lobbies.allow_unidentified(true);
        app.insert_resource(lobbies);
        create(app.world_mut(), &json!({})).expect("create");
        app.world_mut().resource_mut::<regia_net::Lobbies>().pump();
        assert_eq!(
            reason(launch(app.world_mut(), &json!({})).expect_err("no session")),
            "net_not_running"
        );
    }

    fn event_errors(app: &mut App) -> Vec<(String, String)> {
        let st = status(app.world_mut(), &json!({})).expect("status");
        st["recent_events"]
            .as_array()
            .expect("events")
            .iter()
            .filter_map(|e| Some((e["kind"].as_str()?.to_owned(), e["error"].as_str()?.to_owned())))
            .collect()
    }

    /// Spec 89 § 9: `lobby.join` answers at once and a refused join arrives as an `entered`
    /// event with `error: lobby_full` or `lobby_not_found`; the caller stays out of any lobby
    /// and may join again.
    #[test]
    fn refused_joins_report_lobby_full_and_lobby_not_found_as_events() {
        let hub = LocalLobbyHub::new();
        let net = LoopbackNet::new();
        let mut a = app(&hub, &net, 1);
        let mut b = app(&hub, &net, 2);
        create(a.world_mut(), &json!({ "max_members": 1 })).expect("create");
        frames(&mut [&mut a], 1);
        let st = status(a.world_mut(), &json!({})).expect("status");
        let lobby_id = st["lobby"]["id"].as_str().expect("id").to_owned();

        join(b.world_mut(), &json!({ "lobby": lobby_id })).expect("join is asynchronous");
        frames(&mut [&mut a, &mut b], 1);
        assert_eq!(
            event_errors(&mut b),
            vec![("entered".to_owned(), "lobby_full".to_owned())]
        );
        let st = status(b.world_mut(), &json!({})).expect("status");
        assert!(st["lobby"].is_null(), "{st}");

        join(b.world_mut(), &json!({ "lobby": "18446744073709551615" })).expect("join again");
        frames(&mut [&mut b], 1);
        let errors = event_errors(&mut b);
        assert_eq!(
            errors.last(),
            Some(&("entered".to_owned(), "lobby_not_found".to_owned())),
            "{errors:?}"
        );
        let st = status(b.world_mut(), &json!({})).expect("status");
        assert!(st["lobby"].is_null(), "{st}");
        assert_eq!(hub.lobbies().len(), 1);
    }

    /// A transport that cannot open a socket.
    struct NoSocket;

    impl regia_net::lobby::LobbyTransport for NoSocket {
        fn name(&self) -> &'static str {
            "no_socket"
        }

        fn open(&self) -> std::io::Result<Box<dyn regia_net::DatagramSocket + Send>> {
            Err(std::io::Error::other("no socket for this test"))
        }

        fn server_for_host(&self, local: SocketAddr) -> LobbyServer {
            LobbyServer::Address(local)
        }

        fn peer_addr(&self, _server: &LobbyServer) -> Option<SocketAddr> {
            None
        }
    }

    /// Spec 89 § 9: `lobby.launch` whose session cannot host answers `session_error`, and the
    /// lobby stays `open` with no session running.
    #[test]
    fn a_launch_that_cannot_host_answers_session_error() {
        let hub = LocalLobbyHub::new();
        let net = LoopbackNet::new();
        let mut a = app(&hub, &net, 1);
        a.world_mut()
            .resource_mut::<regia_net::Lobbies>()
            .set_transport(Arc::new(NoSocket));
        create(a.world_mut(), &json!({})).expect("create");
        frames(&mut [&mut a], 1);
        assert_eq!(
            reason(launch(a.world_mut(), &json!({})).expect_err("no socket")),
            "session_error"
        );
        frames(&mut [&mut a], 1);
        let st = status(a.world_mut(), &json!({})).expect("status");
        assert_eq!(st["lobby"]["data"]["regia.state"], "open", "{st}");
        assert!(st["lobby"]["server"].is_null(), "{st}");
        assert!(a.world().resource::<NetSession>().server().is_none());
    }

    /// ADR-7881: with no identity (no `LobbyPlugin` session to take it from, nothing set by the
    /// game) `lobby.create` and `lobby.list` answer `lobby_no_identity`, and `lobby.status`
    /// `identity` says why; the opt-in or a game id lets them through.
    #[test]
    fn no_identity_answers_lobby_no_identity_and_status_shows_the_source() {
        let hub = LocalLobbyHub::new();
        let mut bare = App::new();
        bare.insert_resource(regia_net::Lobbies::new(Arc::new(hub.member(1, "p1"))));
        assert_eq!(
            reason(create(bare.world_mut(), &json!({})).expect_err("no identity")),
            "lobby_no_identity"
        );
        assert_eq!(
            reason(list(bare.world_mut(), &json!({})).expect_err("no identity")),
            "lobby_no_identity"
        );
        let st = status(bare.world_mut(), &json!({})).expect("status");
        assert_eq!(st["identity"]["source"], "unset", "{st}");
        assert_eq!(st["identity"]["allow_unidentified"], false);
        bare.world_mut()
            .resource_mut::<regia_net::Lobbies>()
            .set_game("example.arena");
        create(bare.world_mut(), &json!({})).expect("a game id is an identity");
        let st = status(bare.world_mut(), &json!({})).expect("status");
        assert_eq!(st["identity"]["game"], "example.arena");

        // With NetSessionPlugin the plugin takes the session's manifest.
        let net = LoopbackNet::new();
        let mut a = app(&hub, &net, 9);
        let st = status(a.world_mut(), &json!({})).expect("status");
        assert_eq!(st["identity"]["source"], "session", "{st}");
        let content = a.world().resource::<NetSession>().manifest().content_hash;
        assert_eq!(st["identity"]["content"], regia_net::lobby::hash_hex(content));
    }
}
