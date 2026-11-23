// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the `regia_net` model and replication layer (specs 70 and 71,
//! `book/agents/net-model.md`, `book/agents/net-replication.md`, ADR-1643): `net.entity`,
//! `net.messages`, `net.peer_flood`, `net.set_owner`, `net.replicated`, `net.entity_state`,
//! `net.wake`, `net.set_asleep`, `net.resend`, `net.pending_refs`, and the model fields of
//! `net.status`.
//!
//! Handler errors are `F0002` with `data.reason` set to one of: `net_not_running` (the app has no
//! net runtime), `net_not_server` (a server-only method on a client), `entity_not_replicated`,
//! `entity_not_found`, `peer_unknown`, `message_type_unknown`, `component_not_replicated`,
//! `net_dev_only` (`net.set_owner` in a shipping build, ADR-2415). A bad parameter is `F0002`
//! with `data.hint` and no `data.reason`.

use regia_app::prelude::*;
use regia_net::model::{NetMode, NetOwnedLocally, NetOwner, NetPlayer, NetRole, PeerId, resolve_owner};
use regia_net::replication::{
    ClientReplication, NetAsleep, ReplicationRegistry, ServerReplication, SpawnKind,
};
use regia_net::runtime::{NetEntities, NetNode};
use regia_net::{LocalPeer, NetId};
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn not_running() -> RpcError {
    f0002(
        "net_not_running",
        "the app has no regia_net runtime",
        "add regia_net::NetPlugin (or call regia_net::runtime::ensure_resources) and set a NetMode",
    )
}

fn not_server(method: &str) -> RpcError {
    f0002(
        "net_not_server",
        format!("{method} runs on the server; this process is a client"),
        "call it on the server, the listen host or an offline process",
    )
}

fn mode(world: &World) -> Result<NetMode, RpcError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(not_running());
    }
    Ok(world.get_resource::<NetMode>().copied().unwrap_or_default())
}

fn server_mode(world: &World, method: &str) -> Result<(), RpcError> {
    if mode(world)?.is_server() {
        Ok(())
    } else {
        Err(not_server(method))
    }
}

fn entity_param(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let entity = Entity::try_from_bits(bits).ok_or_else(|| {
        f0002(
            "entity_not_found",
            format!("{bits} is not an entity id"),
            "use net.replicated",
        )
    })?;
    if world.get_entity(entity).is_err() {
        return Err(f0002(
            "entity_not_found",
            format!("entity {bits} does not exist"),
            "use net.replicated or scene.query_entities",
        ));
    }
    Ok(entity)
}

fn replicated(world: &World, entity: Entity) -> Result<NetId, RpcError> {
    world.get::<NetId>(entity).copied().ok_or_else(|| {
        f0002(
            "entity_not_replicated",
            format!("entity {} is not replicated", entity.to_bits()),
            "insert regia_net::Replicated on the server; net.replicated lists replicated entities",
        )
    })
}

fn peer_param(params: &Value) -> Result<Option<PeerId>, RpcError> {
    match params.get("peer") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .map(|p| Some(PeerId(p)))
            .ok_or_else(|| RpcError::invalid_params("`peer` must be a peer id (an integer) or null")),
    }
}

/// The model fields `net.status` adds (spec 70 § 9): `{ mode, local_peer, tick, peers, players }`.
/// `None` when the app has no net runtime.
pub(crate) fn status_fields(world: &mut World) -> Option<Map<String, Value>> {
    let node = world.get_resource::<NetNode>()?;
    let mode = world.get_resource::<NetMode>().copied().unwrap_or_default();
    let mut out = Map::new();
    out.insert("mode".into(), json!(mode.as_str()));
    let local = world.get_resource::<LocalPeer>().and_then(|p| p.0).map(|p| p.0);
    out.insert("local_peer".into(), json!(local));
    out.insert("tick".into(), json!(node.tick.0));
    let peers: Vec<u32> = node.peers().iter().map(|p| p.0).collect();
    out.insert("peers".into(), json!(peers));
    let mut q = world.query::<&NetPlayer>();
    let mut players: Vec<(u32, u8)> = q.iter(world).map(|p| (p.id.peer.0, p.id.local)).collect();
    players.sort_unstable();
    let players: Vec<Value> = players
        .into_iter()
        .map(|(peer, local)| json!({ "peer": peer, "local": local }))
        .collect();
    out.insert("players".into(), json!(players));
    // Spec 120 § 11.2 (ADR-7984): what the replication runtime refused and from whom.
    if mode.is_server() {
        let node = world.resource::<NetNode>();
        let (total, dropped) = node.rejection_counts();
        let mut recent: Vec<Value> = node
            .rejections()
            .rev()
            .take(STATUS_RECENT_REJECTIONS)
            .map(|r| json!({ "peer": r.peer.0, "kind": r.kind.as_str(), "detail": r.detail, "tick": r.tick.0 }))
            .collect();
        recent.reverse();
        out.insert(
            "rejections".into(),
            json!({ "total": total, "dropped": dropped, "recent": recent }),
        );
    }
    Some(out)
}

/// Newest runtime refusals `net.status` lists.
const STATUS_RECENT_REJECTIONS: usize = 16;

/// `net.entity`: an entity's network id, role, owner (server), local ownership and the peers it is
/// relevant to (server).
pub(crate) fn entity(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let is_server = mode(world)?.is_server();
    let e = entity_param(world, params)?;
    let id = replicated(world, e)?;
    let role = world.get::<NetRole>(e).map(|r| r.as_str());
    let owned_locally = world.get::<NetOwnedLocally>(e).is_some();
    if is_server {
        let owner = resolve_owner(world, e).peer().0;
        let relevant: Vec<u32> = world
            .get_resource::<ServerReplication>()
            .map(|r| r.holders(id).iter().map(|p| p.0).collect())
            .unwrap_or_default();
        Ok(json!({
            "net_id": id.0,
            "role": role,
            "owner": owner,
            "owned_locally": owned_locally || owner == PeerId::SERVER.0,
            "relevant_to": relevant,
        }))
    } else {
        Ok(json!({
            "net_id": id.0,
            "role": role,
            "owner": Value::Null,
            "owned_locally": owned_locally,
            "relevant_to": Value::Null,
        }))
    }
}

/// `net.messages`: the per-type message counters, or one type's.
pub(crate) fn messages(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    mode(world)?;
    let Some(reg) = world.get_resource::<regia_net::message::NetMessageRegistry>() else {
        return Err(not_running());
    };
    let filter = match params.get("type") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(reg.find(name).ok_or_else(|| {
            f0002(
                "message_type_unknown",
                format!("no registered net message type `{name}`"),
                "net.messages without `type` lists the registered types",
            )
        })?),
        Some(_) => return Err(RpcError::invalid_params("`type` must be a string")),
    };
    let rows: Vec<Value> = reg
        .types()
        .iter()
        .enumerate()
        .filter(|(i, _)| filter.is_none_or(|f| usize::from(f) == *i))
        .map(|(i, t)| {
            let c = reg.counters.get(i).copied().unwrap_or_default();
            json!({
                "type": t.name,
                "direction": t.direction.as_str(),
                "channel": t.channel.as_str(),
                "sent": c.sent,
                "received": c.received,
                "delivered": c.delivered,
                "rejected": c.rejected,
                "rate_limited": c.rate_limited,
                "not_owner": c.not_owner,
                "unresolved": c.unresolved,
                "dropped_overflow": c.dropped_overflow,
                "late": c.late,
                "bytes_sent": c.bytes_sent,
                "bytes_received": c.bytes_received,
                // ADR-2415: the schema version and the stable id (hex, it is a u64).
                "version": t.version,
                "stable_id": format!("{:016x}", t.stable_id),
            })
        })
        .collect();
    Ok(json!({ "types": rows }))
}

/// `net.peer_flood`: a peer's flood state on the server.
pub(crate) fn peer_flood(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server_mode(world, "net.peer_flood")?;
    let peer = peer_param(params)?.ok_or_else(|| RpcError::invalid_params("`peer` is required"))?;
    let node = world.get_resource::<NetNode>().ok_or_else(not_running)?;
    let shed = node.peer_inbound_shed(peer).unwrap_or(0);
    let Some((state, breaches, last)) = node.peer_flood(peer) else {
        return Err(f0002(
            "peer_unknown",
            format!("no connected peer {}", peer.0),
            "net.status lists the connected peers",
        ));
    };
    Ok(json!({
        "state": state.as_str(),
        "breaches_last_second": breaches,
        "last_breach_tick": last.map(|t| t.0),
        "inbound_shed": shed,
    }))
}

/// Whether this is a development build, where `net.set_owner` exists (spec 70 § 9: "server, dev
/// builds"). Shipping builds are built without debug assertions.
pub(crate) const SET_OWNER_AVAILABLE: bool = cfg!(debug_assertions);

/// The dev-build gate of `net.set_owner`. `allowed` is the build's answer
/// (`SET_OWNER_AVAILABLE`), a parameter so a test sees the shipping refusal: `F0002` with
/// `data.reason` `net_dev_only`.
pub(crate) fn set_owner_gate(allowed: bool) -> Result<(), RpcError> {
    dev_only_gate(
        allowed,
        "net.set_owner",
        "run a development build; in a shipping game ownership changes through gameplay code (NetOwner)",
    )
}

/// The dev-build gate every `net.*` method that specs mark "dev builds" goes through
/// (`net.set_owner`, spec 70 § 9; `net.set_interp_delay`, spec 74 § 12; `net.force_correction`,
/// spec 73 § 12; ADR-5993): `F0002` with `data.reason` `net_dev_only` when `allowed` is false.
pub(crate) fn dev_only_gate(allowed: bool, method: &str, remedy: &str) -> Result<(), RpcError> {
    if allowed {
        Ok(())
    } else {
        Err(f0002(
            "net_dev_only",
            format!("{method} is not available in a shipping build"),
            remedy,
        ))
    }
}

/// `net.set_owner`: sets (or, with `peer: null`, clears) an entity's owner on the server.
/// Development builds only.
pub(crate) fn set_owner(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    set_owner_gate(SET_OWNER_AVAILABLE)?;
    server_mode(world, "net.set_owner")?;
    let e = entity_param(world, params)?;
    replicated(world, e)?;
    let peer = peer_param(params)?;
    if let Some(p) = peer
        && p != PeerId::SERVER
        && !world.get_resource::<NetNode>().is_some_and(|n| n.has_peer(p))
    {
        return Err(f0002(
            "peer_unknown",
            format!("no connected peer {}", p.0),
            "net.status lists the connected peers",
        ));
    }
    let mut ent = world.entity_mut(e);
    match peer {
        Some(p) => {
            ent.insert(NetOwner(p));
        }
        None => {
            ent.remove::<NetOwner>();
        }
    }
    Ok(json!({ "owner": resolve_owner(world, e).peer().0 }))
}

fn spawn_kind_json(kind: SpawnKind) -> Value {
    match kind {
        SpawnKind::Bare => Value::Null,
        SpawnKind::Prefab(p) => json!(format!("{p:032x}")),
        SpawnKind::Level { guid, .. } => json!(format!("level:{guid:032x}")),
    }
}

/// `net.replicated`: the replicated entities, with an optional filter and limit.
pub(crate) fn replicated_list(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let is_server = mode(world)?.is_server();
    let filter = params.get("filter").filter(|f| !f.is_null());
    let registry = world
        .get_resource::<ReplicationRegistry>()
        .cloned()
        .unwrap_or_default();
    let component = match filter.and_then(|f| f.get("component")) {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(registry.find(name).ok_or_else(|| {
            f0002(
                "component_not_replicated",
                format!("`{name}` is not a replicated component"),
                "register it with app.replicate::<C>(..); net.replicated lists components per entity",
            )
        })?),
        Some(_) => return Err(RpcError::invalid_params("`filter.component` must be a string")),
    };
    let asleep_filter = match filter.and_then(|f| f.get("asleep")) {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(RpcError::invalid_params("`filter.asleep` must be a boolean")),
    };
    let owner_filter = match filter.and_then(|f| f.get("owner")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .ok_or_else(|| RpcError::invalid_params("`filter.owner` must be a peer id"))?,
        ),
    };
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 1000,
        Some(v) => v
            .as_u64()
            .map(|l| usize::try_from(l).unwrap_or(usize::MAX))
            .ok_or_else(|| RpcError::invalid_params("`limit` must be a non-negative integer"))?,
    };
    // The server reads its replication table. A client reads the copies: sleep from `NetAsleep`,
    // and prefab or level binding from the components on the copy (spec 71 § 4.4).
    let rows: Vec<(NetId, Entity, bool, Option<SpawnKind>)> = if is_server {
        world
            .get_resource::<ServerReplication>()
            .map(|r| r.entities().map(|(i, e, a, k)| (i, e, a, Some(k))).collect())
            .unwrap_or_default()
    } else {
        let ids: Vec<(NetId, Entity)> = world
            .get_resource::<NetEntities>()
            .map(|m| m.iter().collect())
            .unwrap_or_default();
        ids.into_iter()
            .map(|(i, e)| {
                let asleep = world.get::<NetAsleep>(e).is_some();
                let kind = regia_net::replication::spawn_kind_of(world, e);
                (i, e, asleep, Some(kind))
            })
            .collect()
    };
    // A client names a component from the last applied value (`NetReceived` while the copy still
    // holds it). The server names the live component. The annotation makes the two methods one
    // function-pointer type.
    let component_text: fn(&ReplicationRegistry, &World, u16, Entity) -> Option<String> = if is_server {
        ReplicationRegistry::debug_value
    } else {
        ReplicationRegistry::debug_value_applied
    };
    let mut out = Vec::new();
    for (id, e, asleep, kind) in rows {
        if out.len() >= limit {
            break;
        }
        let names: Vec<&str> = registry
            .components()
            .iter()
            .enumerate()
            .filter(|(i, _)| component_text(&registry, world, *i as u16, e).is_some())
            .map(|(_, c)| c.name.rsplit("::").next().unwrap_or(c.name))
            .collect();
        if let Some(ci) = component
            && component_text(&registry, world, ci, e).is_none()
        {
            continue;
        }
        if asleep_filter.is_some_and(|a| a != asleep) {
            continue;
        }
        let owner = is_server.then(|| resolve_owner(world, e).peer().0);
        if owner_filter.is_some_and(|o| Some(o) != owner) {
            continue;
        }
        out.push(json!({
            "entity": e.to_bits(),
            "net_id": id.0,
            "prefab": kind.map_or(Value::Null, spawn_kind_json),
            "components": names,
            "asleep": asleep,
            "owner": owner,
        }));
    }
    Ok(json!({ "entities": out }))
}

/// `net.entity_state`: the replicated component values as last applied (client) or as they are
/// now on the server, with the tick.
///
/// On a client the tick is the newest state frame that carried this entity, or the client's
/// state tick when that entity has no stored state record. On the state channel that state tick
/// is the newest state frame; otherwise it is the newest applied tick. A component's text is the
/// newest received value while the copy still holds `NetReceived`, and the live component
/// otherwise. The server reports the live values and the tick of that entity's last change.
pub(crate) fn entity_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let is_server = mode(world)?.is_server();
    let e = entity_param(world, params)?;
    let id = replicated(world, e)?;
    let registry = world
        .get_resource::<ReplicationRegistry>()
        .cloned()
        .unwrap_or_default();
    let component_text: fn(&ReplicationRegistry, &World, u16, Entity) -> Option<String> = if is_server {
        ReplicationRegistry::debug_value
    } else {
        ReplicationRegistry::debug_value_applied
    };
    let mut values = Map::new();
    for (i, c) in registry.components().iter().enumerate() {
        if let Some(v) = component_text(&registry, world, i as u16, e) {
            values.insert(c.name.rsplit("::").next().unwrap_or(c.name).into(), json!(v));
        }
    }
    let tick = if is_server {
        world
            .get_resource::<ServerReplication>()
            .and_then(|r| r.last_change(id))
            .map(|t| t.0)
    } else {
        world
            .get_resource::<ClientReplication>()
            .and_then(|r| r.state.carried_tick(id).or_else(|| r.state_tick()).map(|t| t.0))
    };
    Ok(json!({ "net_id": id.0, "tick": tick, "components": values }))
}

/// `net.wake`: wakes an asleep entity (server).
pub(crate) fn wake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server_mode(world, "net.wake")?;
    let e = entity_param(world, params)?;
    replicated(world, e)?;
    regia_net::net_wake(world, e);
    Ok(json!({ "asleep": false }))
}

/// `net.set_asleep`: forces an entity asleep (server); it stops sending once every connection
/// acknowledged its latest state.
pub(crate) fn set_asleep(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server_mode(world, "net.set_asleep")?;
    let e = entity_param(world, params)?;
    let id = replicated(world, e)?;
    world.entity_mut(e).insert(NetAsleep);
    let pending = world
        .get_resource::<ServerReplication>()
        .map_or(0, |r| r.pending_acks(id));
    Ok(json!({ "asleep": true, "pending_acks": pending }))
}

/// `net.resend`: sends an entity's current state once to every relevant connection (server).
pub(crate) fn resend(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server_mode(world, "net.resend")?;
    let e = entity_param(world, params)?;
    replicated(world, e)?;
    let peers: Vec<u32> = regia_net::net_resend(world, e).iter().map(|p| p.0).collect();
    Ok(json!({ "queued_to": peers }))
}

/// `net.pending_refs`: entity references a client is waiting to resolve.
pub(crate) fn pending_refs(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    mode(world)?;
    let registry = world
        .get_resource::<ReplicationRegistry>()
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Value> = world
        .get_resource::<ClientReplication>()
        .map(|c| {
            c.pending_refs
                .iter()
                .map(|p| {
                    let name = registry.get(p.component).map_or("?", |c| c.name);
                    json!({
                        "entity": p.entity.to_bits(),
                        "component": name.rsplit("::").next().unwrap_or(name),
                        "field": Value::Null,
                        "waiting_for_net_id": p.waiting_for.0,
                        "since_tick": p.since.0,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "pending": rows }))
}

/// Default and largest `limit` of `net.assets`.
const ASSETS_LIMIT: (u64, u64) = (100, 1024);

/// `net.assets` (spec 71 § 3, ADR-2415): whether this process has an asset-server context for
/// replicated `Handle<T>` fields, how many handles it sent as none because its resolver did not
/// know them, and the asset keys that did not resolve here, each with its code.
pub(crate) fn assets(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    mode(world)?;
    let limit = match params.get("limit") {
        None | Some(Value::Null) => ASSETS_LIMIT.0,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=ASSETS_LIMIT.1).contains(n))
            .ok_or_else(|| RpcError::invalid_params("`limit` must be an integer from 1 to 1024"))?,
    };
    let Some(a) = world.get_resource::<regia_net::assets::NetAssets>() else {
        return Ok(json!({
            "resolver": false,
            "unmapped": 0,
            "missing": [],
            "missing_total": 0,
            "missing_overflow": 0,
        }));
    };
    let missing: Vec<Value> = a
        .missing()
        .take(limit as usize)
        .map(|(key, m)| {
            json!({
                "key": key.to_string(),
                "code": m.code,
                "count": m.count,
                "first_tick": m.first_tick.0,
            })
        })
        .collect();
    Ok(json!({
        "resolver": a.context().is_some(),
        "unmapped": a.unmapped(),
        "missing": missing,
        "missing_total": a.missing().count(),
        "missing_overflow": a.missing_overflow,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::prelude::Component;
    use regia_net::replication::register_replicated;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetMessage, ReplicateOptions, Replicated, ToServerMessage};

    #[derive(Component, Clone, Debug, PartialEq, Default, regia_net::Replicate)]
    struct Hp(u32);

    #[derive(Component, Clone, Debug, PartialEq, regia_net::Replicate)]
    struct Aim(Entity);

    #[derive(Clone, Debug, PartialEq, regia_net::Replicate)]
    struct Ping;
    impl NetMessage for Ping {
        const DIRECTION: regia_net::Direction = regia_net::Direction::ToServer;
    }
    impl ToServerMessage for Ping {}

    fn setup(world: &mut World) {
        register_replicated::<Hp>(world, ReplicateOptions::default());
        register_replicated::<Aim>(world, ReplicateOptions::default());
        regia_net::message::register_net_message::<Ping>(world);
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    fn rep() -> Replicated {
        Replicated {
            transform: regia_net::TransformReplication::None,
            ..Replicated::default()
        }
    }

    /// Spec 120 § 11.2 (ADR-7984): `net.status` on a server names what the runtime refused and from
    /// whom, newest 16, without taking them from the host; a client answers no `rejections`.
    #[test]
    fn net_status_lists_runtime_rejections_on_the_server() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 3, setup);
        let a = hub.add_client();
        let peer = hub.clients[a].peer;
        hub.run(3);
        let sv = &mut hub.server.world;
        let status = crate::net::status(sv, &json!({})).expect("status");
        assert_eq!(
            status["rejections"],
            json!({ "total": 0, "dropped": 0, "recent": [] })
        );
        {
            let mut node = sv.resource_mut::<NetNode>();
            for _ in 0..20 {
                assert!(node.reject(peer, regia_net::RejectKind::VoiceMalformed, "voice packet"));
            }
            assert!(!node.reject(PeerId(999), regia_net::RejectKind::VoiceMalformed, "voice packet"));
        }
        let status = crate::net::status(sv, &json!({})).expect("status");
        crate::contract::assert_result_named("net.status", &status);
        let r = &status["rejections"];
        assert_eq!(
            (r["total"].as_u64(), r["dropped"].as_u64()),
            (Some(20), Some(0)),
            "{status}"
        );
        let recent = r["recent"].as_array().expect("recent");
        assert_eq!(recent.len(), 16);
        assert_eq!(recent[0]["peer"], peer.0);
        assert_eq!(recent[0]["kind"], "voice_malformed");
        assert_eq!(recent[0]["detail"], "voice packet");
        assert_eq!(
            sv.resource::<NetNode>().rejections().len(),
            20,
            "net.status takes nothing"
        );
        let client = crate::net::status(&mut hub.clients[a].world, &json!({})).expect("status");
        assert!(client.get("rejections").is_none(), "{client}");
    }

    /// Every spec 70 / 71 method round-trips in a loopback session and each error reason is
    /// produced (spec 70 § 10 test 14, spec 71 § 14 test 14).
    #[test]
    fn net_model_methods_round_trip_and_every_reason() {
        let mut hub = LoopbackHub::new(NetMode::ListenServer, 3, setup);
        let a = hub.add_client();
        let peer = hub.clients[a].peer;
        let target = hub.server.world.spawn((rep(), Hp(5))).id();
        hub.server
            .world
            .insert_resource(regia_net::replication::NetRelevance {
                filter: Some(|world: &World, e: Entity, _| world.get::<Hp>(e).is_none_or(|h| h.0 != 404)),
            });
        let e = hub.server.world.spawn((rep(), Hp(7), NetOwner(peer))).id();
        let referrer = hub.server.world.spawn((rep(), Aim(target))).id();
        let local = hub.server.world.spawn(Hp(1)).id();
        hub.run(4);
        let sv = &mut hub.server.world;
        let bits = json!({ "entity": e.to_bits() });

        let status = crate::net::status(sv, &json!({})).expect("status");
        assert_eq!(status["mode"], "listen_server");
        assert_eq!(status["peers"], json!([peer.0]));
        assert_eq!(status["local_peer"], 0);
        // Spec 70 § 9 fields (lane spec-28-68): the tick and the player list.
        assert!(status["tick"].as_u64().is_some_and(|t| t >= 4), "{status}");
        assert!(status["players"].is_array(), "{status}");

        let ent = entity(sv, &bits).expect("entity");
        assert_eq!(ent["owner"], peer.0);
        assert_eq!(ent["role"], "authority");
        assert_eq!(ent["relevant_to"], json!([peer.0]));
        let msgs = messages(sv, &json!({ "type": "Ping" })).expect("messages");
        assert_eq!(msgs["types"][0]["direction"], "to_server");
        crate::contract::assert_result_named("net.messages", &msgs);
        let flood = peer_flood(sv, &json!({ "peer": peer.0 })).expect("flood");
        assert_eq!(flood["state"], "clear");
        let owner = set_owner(sv, &json!({ "entity": e.to_bits(), "peer": null })).expect("owner");
        assert_eq!(owner["owner"], 0);
        // Spec 70 § 9: `net.set_owner` to a peer that is not connected, then back to the client.
        let err = set_owner(sv, &json!({ "entity": e.to_bits(), "peer": 77 })).expect_err("unknown peer");
        assert_eq!(reason(&err).as_deref(), Some("peer_unknown"));
        let owner = set_owner(sv, &json!({ "entity": e.to_bits(), "peer": peer.0 })).expect("owner");
        assert_eq!(owner["owner"], peer.0);
        let owner = set_owner(sv, &json!({ "entity": e.to_bits(), "peer": null })).expect("owner");
        assert_eq!(owner["owner"], 0);
        let list = replicated_list(sv, &json!({ "filter": { "component": "Hp" } })).expect("list");
        assert_eq!(list["entities"].as_array().map(Vec::len), Some(2));
        let state = entity_state(sv, &bits).expect("state");
        assert_eq!(state["components"]["Hp"], "Hp(7)");
        let asleep = set_asleep(sv, &bits).expect("asleep");
        assert_eq!(asleep["asleep"], true);
        assert_eq!(wake(sv, &bits).expect("wake")["asleep"], false);
        assert_eq!(resend(sv, &bits).expect("resend")["queued_to"], json!([peer.0]));

        // Errors on the server.
        let err = entity(sv, &json!({ "entity": local.to_bits() })).expect_err("local");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_replicated"));
        let gone = sv.spawn_empty().id();
        assert!(sv.despawn(gone));
        let err = entity(sv, &json!({ "entity": gone.to_bits() })).expect_err("gone");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_found"));
        let err = peer_flood(sv, &json!({ "peer": 77 })).expect_err("peer");
        assert_eq!(reason(&err).as_deref(), Some("peer_unknown"));
        let err = messages(sv, &json!({ "type": "Nope" })).expect_err("type");
        assert_eq!(reason(&err).as_deref(), Some("message_type_unknown"));
        let err = replicated_list(sv, &json!({ "filter": { "component": "Nope" } })).expect_err("comp");
        assert_eq!(reason(&err).as_deref(), Some("component_not_replicated"));

        // The client: pending references, and server-only methods refused.
        sv.get_mut::<Hp>(target).expect("hp").0 = 404;
        sv.get_mut::<Aim>(referrer).expect("aim").0 = target;
        hub.run(6);
        let cw = &mut hub.clients[a].world;
        let id = *hub.server.world.get::<NetId>(e).expect("id");
        let ce = cw.resource::<NetEntities>().entity(id).expect("copy");
        let err = wake(cw, &json!({ "entity": ce.to_bits() })).expect_err("client");
        assert_eq!(reason(&err).as_deref(), Some("net_not_server"));
        // Spec 70 § 9: the other server-only methods refuse on a client too.
        let err = peer_flood(cw, &json!({ "peer": 0 })).expect_err("client flood");
        assert_eq!(reason(&err).as_deref(), Some("net_not_server"));
        let err = set_owner(cw, &json!({ "entity": ce.to_bits(), "peer": null })).expect_err("client owner");
        assert_eq!(reason(&err).as_deref(), Some("net_not_server"));
        let client_status = crate::net::status(cw, &json!({})).expect("client status");
        assert_eq!(client_status["mode"], "client");
        let pending = pending_refs(cw, &json!({})).expect("pending");
        assert_eq!(pending["pending"].as_array().map(Vec::len), Some(1));
        assert_eq!(pending["pending"][0]["component"], "Aim");
        let ent = entity(cw, &json!({ "entity": ce.to_bits() })).expect("client entity");
        assert_eq!(ent["role"], "proxy");

        // No runtime at all.
        let mut bare = World::new();
        let err = messages(&mut bare, &json!({})).expect_err("bare");
        assert_eq!(reason(&err).as_deref(), Some("net_not_running"));
    }

    #[test]
    fn replicated_list_filters_limit_and_types() {
        let mut hub = LoopbackHub::new(NetMode::ListenServer, 3, setup);
        let a = hub.add_client();
        let peer = hub.clients[a].peer;
        let owned = hub.server.world.spawn((rep(), Hp(1), NetOwner(peer))).id();
        let plain = hub.server.world.spawn((rep(), Hp(2))).id();
        let sleeper = hub.server.world.spawn((rep(), Hp(3))).id();
        hub.run(4);
        set_asleep(&mut hub.server.world, &json!({ "entity": sleeper.to_bits() })).expect("asleep");
        hub.run(2);
        let sv = &mut hub.server.world;
        let count = |sv: &mut World, p: Value| {
            replicated_list(sv, &p).map(|v| v["entities"].as_array().map_or(0, Vec::len))
        };
        assert_eq!(count(sv, json!({})).expect("all"), 3);
        assert_eq!(
            count(sv, json!({ "limit": 0 })).expect("zero"),
            0,
            "limit 0 lists nothing"
        );
        assert_eq!(count(sv, json!({ "limit": 2 })).expect("two"), 2);
        assert_eq!(count(sv, json!({ "limit": u64::MAX })).expect("max"), 3);
        assert_eq!(
            count(sv, json!({ "filter": { "asleep": true } })).expect("asleep"),
            1
        );
        assert_eq!(
            count(sv, json!({ "filter": { "asleep": false } })).expect("awake"),
            2
        );
        assert_eq!(
            count(sv, json!({ "filter": { "owner": peer.0 } })).expect("owned"),
            1
        );
        assert_eq!(
            count(sv, json!({ "filter": { "owner": 0 } })).expect("server-owned"),
            2
        );
        assert_eq!(count(sv, json!({ "filter": null })).expect("null filter"), 3);
        // Wrong types are refused instead of falling back to "everything".
        for bad in [
            json!({ "limit": -1 }),
            json!({ "limit": "ten" }),
            json!({ "limit": 1.5 }),
            json!({ "filter": { "asleep": "yes" } }),
            json!({ "filter": { "owner": -1 } }),
            json!({ "filter": { "owner": 4_294_967_296_u64 } }),
            json!({ "filter": { "component": 3 } }),
        ] {
            let err = replicated_list(sv, &bad).expect_err("type");
            assert_eq!(err.code, INVALID_PARAMS, "{bad}");
        }
        let _ = (owned, plain);
        // The order is stable across repeated reads.
        let first = replicated_list(sv, &json!({})).expect("a");
        assert_eq!(first, replicated_list(sv, &json!({})).expect("b"));
        // set_owner: unknown peers and out-of-range ids are refused, and the owner is kept.
        let err = set_owner(sv, &json!({ "entity": plain.to_bits(), "peer": 99 })).expect_err("peer");
        assert_eq!(reason(&err).as_deref(), Some("peer_unknown"));
        assert!(
            set_owner(
                sv,
                &json!({ "entity": plain.to_bits(), "peer": 4_294_967_296_u64 })
            )
            .is_err()
        );
        assert!(set_owner(sv, &json!({ "entity": plain.to_bits(), "peer": "a" })).is_err());
        assert_eq!(
            entity(sv, &json!({ "entity": plain.to_bits() })).expect("entity")["owner"],
            0
        );
        assert!(entity(sv, &json!({})).is_err());
        assert!(entity(sv, &json!({ "entity": "7" })).is_err());
        assert_eq!(
            reason(&entity(sv, &json!({ "entity": 0 })).expect_err("invalid bits")).as_deref(),
            Some("entity_not_found")
        );
    }

    fn client_world() -> World {
        let mut world = World::new();
        world.insert_resource(NetMode::Client);
        regia_net::runtime::ensure_resources(&mut world);
        register_replicated::<Hp>(&mut world, ReplicateOptions::default());
        world
    }

    /// A client copy reports its prefab or level binding and its sleep flag. `owner` stays null,
    /// so `filter.owner` matches nothing there.
    #[test]
    fn client_replicated_list_reports_prefab_level_and_sleep() {
        use regia_core::id::Guid;
        use regia_scene::components::EntityGuid;
        use regia_scene::level::LevelMember;
        use regia_scene::prefab::PrefabInstance;

        let mut world = client_world();
        let bare = world.spawn(Hp(1)).id();
        let level_guid = 0x1111_u128;
        let level = world
            .spawn((
                Hp(2),
                LevelMember(0x2222),
                EntityGuid(Guid::from_u128(level_guid)),
                NetAsleep,
            ))
            .id();
        let prefab_id = 0xabcd_u128;
        let prefab = world
            .spawn((
                Hp(3),
                PrefabInstance {
                    prefab: prefab_id,
                    ..PrefabInstance::default()
                },
            ))
            .id();
        world.entity_mut(bare).insert(NetId(1));
        world.entity_mut(level).insert(NetId(2));
        world.entity_mut(prefab).insert(NetId(3));
        {
            let mut map = world.resource_mut::<NetEntities>();
            map.insert(NetId(1), bare);
            map.insert(NetId(2), level);
            map.insert(NetId(3), prefab);
        }

        let list = replicated_list(&mut world, &json!({})).expect("list");
        let rows = list["entities"].as_array().expect("rows");
        assert_eq!(rows.len(), 3);
        assert!(rows[0]["prefab"].is_null());
        assert_eq!(rows[0]["asleep"], false);
        assert!(rows[0]["owner"].is_null());
        assert_eq!(rows[0]["components"][0], "Hp");
        let level_prefab = format!("level:{level_guid:032x}");
        assert_eq!(rows[1]["prefab"].as_str(), Some(level_prefab.as_str()));
        assert_eq!(rows[1]["asleep"], true);
        let prefab_hex = format!("{prefab_id:032x}");
        assert_eq!(rows[2]["prefab"].as_str(), Some(prefab_hex.as_str()));
        assert_eq!(rows[2]["asleep"], false);

        let asleep = replicated_list(&mut world, &json!({ "filter": { "asleep": true } })).expect("asleep");
        assert_eq!(asleep["entities"].as_array().map(Vec::len), Some(1));
        assert_eq!(asleep["entities"][0]["net_id"], 2);
        let awake = replicated_list(&mut world, &json!({ "filter": { "asleep": false } })).expect("awake");
        let ids: Vec<u64> = awake["entities"]
            .as_array()
            .expect("awake rows")
            .iter()
            .map(|row| row["net_id"].as_u64().expect("id"))
            .collect();
        assert_eq!(ids, vec![1, 3]);
        let owned = replicated_list(&mut world, &json!({ "filter": { "owner": 0 } })).expect("owner");
        assert_eq!(owned["entities"].as_array().map(Vec::len), Some(0));
    }

    /// The client tick is the state channel, not a tick frame that ran ahead of it, and a held
    /// `NetReceived` value is the one reported.
    #[test]
    fn client_entity_state_uses_state_tick_and_received_value() {
        let mut world = client_world();
        let e = world.spawn((Hp(1), NetId(7))).id();
        world.resource_mut::<NetEntities>().insert(NetId(7), e);
        {
            let mut client = world.resource_mut::<ClientReplication>();
            client.applied = Some(regia_net::NetTick(102));
            client.state.newest_tick = Some(regia_net::NetTick(100));
        }
        let state = entity_state(&mut world, &json!({ "entity": e.to_bits() })).expect("state");
        assert_eq!(state["tick"], 100, "a state frame behind `applied` is the tick");
        assert_eq!(state["components"]["Hp"], "Hp(1)");

        world.entity_mut(e).insert(regia_net::interp::NetReceived(Hp(2)));
        {
            let mut client = world.resource_mut::<ClientReplication>();
            client.state.newest_tick = None;
            client.applied = Some(regia_net::NetTick(50));
        }
        let state = entity_state(&mut world, &json!({ "entity": e.to_bits() })).expect("received");
        assert_eq!(
            state["tick"], 50,
            "with no state channel the applied tick is the state tick"
        );
        assert_eq!(state["components"]["Hp"], "Hp(2)");
        let listed = replicated_list(&mut world, &json!({})).expect("listed");
        assert_eq!(listed["entities"][0]["components"][0], "Hp");
    }

    /// ADR-2415: `net.set_owner` is refused under the shipping cfg with `net_dev_only`, before it
    /// reads its parameters or touches the world; a development build passes the gate.
    #[test]
    fn set_owner_is_refused_in_shipping_builds() {
        let err = set_owner_gate(false).expect_err("shipping refuses");
        assert_eq!(err.code, INVALID_PARAMS);
        assert_eq!(reason(&err).as_deref(), Some("net_dev_only"));
        assert!(err.message.contains("shipping"), "{}", err.message);
        assert!(set_owner_gate(true).is_ok());
        // The method itself follows the build: this test binary has debug assertions on unless
        // it was built for release, and the answer matches the gate either way.
        assert_eq!(SET_OWNER_AVAILABLE, cfg!(debug_assertions));
        let mut world = World::new();
        let out = set_owner(&mut world, &json!({}));
        match out {
            Err(e) if !SET_OWNER_AVAILABLE => assert_eq!(reason(&e).as_deref(), Some("net_dev_only")),
            Err(e) => assert_eq!(reason(&e).as_deref(), Some("net_not_running")),
            Ok(v) => panic!("no net runtime, yet set_owner answered {v}"), // invariant: test-only (cfg(test)); names the unexpected answer
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Mesh;

    #[derive(Component, Clone, Debug, PartialEq, regia_net::Replicate)]
    struct Look(regia_asset::Handle<Mesh>);

    #[derive(Clone, Debug, PartialEq, regia_net::NetMessage)]
    #[net(direction = "broadcast", id = "test.chime", version = 4)]
    struct Chime {
        n: u8,
    }

    fn setup_assets(world: &mut World) {
        register_replicated::<Look>(world, ReplicateOptions::default());
        regia_net::message::register_net_message::<Chime>(world);
    }

    /// ADR-2415: `net.assets` lists a replicated handle the client could not resolve, with its
    /// code; `net.messages` reports a derived message's id, version and stable id.
    #[test]
    fn net_assets_lists_unresolved_handles_and_messages_report_versions() {
        use regia_net::assets::{NetAssetTable, NetAssets};
        use std::sync::Arc;
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 4, setup_assets);
        let a = hub.add_client();
        let mut slab = regia_asset::Assets::<Mesh>::new();
        let id = regia_asset::AssetId(regia_core::id::Guid::from_u128(0xabc));
        let h = slab.insert(id, Mesh);
        let table = Arc::new(NetAssetTable::new());
        table.insert_id(&h, id);
        hub.server.world.insert_resource(NetAssets::new(table));
        // The client has an empty table: the asset is unknown there.
        hub.clients[a]
            .world
            .insert_resource(NetAssets::new(Arc::new(NetAssetTable::new())));
        hub.server.world.spawn((rep(), Look(h)));
        hub.run(4);

        let cl = &mut hub.clients[a].world;
        let out = assets(cl, &json!({})).expect("assets");
        assert_eq!(out["resolver"], true);
        assert_eq!(out["missing_total"], 1);
        assert_eq!(out["missing"][0]["key"], format!("{:032x}", 0xabc));
        assert_eq!(out["missing"][0]["code"], "asset_unknown");
        assert!(out["missing"][0]["count"].as_u64() >= Some(1));
        assert!(assets(cl, &json!({ "limit": 0 })).is_err());
        assert!(assets(cl, &json!({ "limit": "x" })).is_err());
        assert_eq!(
            assets(cl, &json!({ "limit": 1 })).expect("limit")["missing"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let sv = &mut hub.server.world;
        let out = assets(sv, &json!({})).expect("server");
        assert_eq!(out["unmapped"], 0);
        assert_eq!(out["missing_total"], 0);
        // A process without the resource answers an empty report; one without a net runtime an error.
        sv.remove_resource::<NetAssets>();
        assert_eq!(assets(sv, &json!({})).expect("none")["resolver"], false);
        assert_eq!(
            reason(&assets(&mut World::new(), &json!({})).expect_err("no net")).as_deref(),
            Some("net_not_running")
        );

        let msgs = messages(sv, &json!({ "type": "test.chime" })).expect("by id");
        assert_eq!(msgs["types"][0]["type"], "test.chime");
        assert_eq!(msgs["types"][0]["version"], 4);
        assert_eq!(
            msgs["types"][0]["stable_id"],
            format!("{:016x}", regia_net::message::message_stable_id("test.chime"))
        );
        assert_eq!(msgs["types"][0]["direction"], "broadcast");
    }

    /// Spec 71 § 13: each per-entity method (`net.entity_state`, `net.wake`, `net.set_asleep`,
    /// `net.resend`) answers `entity_not_replicated` for a local entity, `entity_not_found` for a
    /// despawned one and `net_not_running` without a net runtime.
    #[test]
    fn spec_71_entity_methods_produce_their_error_reasons() {
        type Method = fn(&mut World, &Value) -> Result<Value, RpcError>;
        let methods: [(&str, Method); 4] = [
            ("net.entity_state", entity_state),
            ("net.wake", wake),
            ("net.set_asleep", set_asleep),
            ("net.resend", resend),
        ];
        let mut hub = LoopbackHub::new(NetMode::ListenServer, 4, setup);
        let _a = hub.add_client();
        let local = hub.server.world.spawn(Hp(1)).id();
        hub.run(2);
        let sv = &mut hub.server.world;
        let gone = sv.spawn_empty().id();
        assert!(sv.despawn(gone));
        for (name, method) in methods {
            let err = method(sv, &json!({ "entity": local.to_bits() })).expect_err(name);
            assert_eq!(reason(&err).as_deref(), Some("entity_not_replicated"), "{name}");
            let err = method(sv, &json!({ "entity": gone.to_bits() })).expect_err(name);
            assert_eq!(reason(&err).as_deref(), Some("entity_not_found"), "{name}");
        }
        let mut bare = World::new();
        for (name, method) in methods {
            let err = method(&mut bare, &json!({ "entity": local.to_bits() })).expect_err(name);
            assert_eq!(reason(&err).as_deref(), Some("net_not_running"), "{name}");
        }
    }
}
