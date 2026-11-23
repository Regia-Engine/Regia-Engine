// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of spec 72 (relevance, priority, budgets; `book/agents/net-relevance.md`,
//! ADR-1662): `net.relevance`, `net.why_not_relevant`, `net.bandwidth`, `net.set_client_rate`,
//! `net.profiles`, `net.grant_relevance`.
//!
//! Errors are `F0002` with `data.reason` set to one of: `net_not_running`, `net_not_server`,
//! `peer_unknown`, `entity_not_replicated`, `entity_not_found`, `profile_invalid`,
//! `rate_out_of_range`, `interest_off` (the server has no relevance pipeline installed; only the
//! methods that change its state need it), `net_dev_only` (`net.set_client_rate` and
//! `net.grant_relevance` in a shipping build; spec 72).

use regia_app::prelude::*;
use regia_net::model::{NetMode, NetTick, PeerId};
use regia_net::profile::{NetProfiles, RelevanceShape};
use regia_net::relevance::{GrantTarget, NetInterest};
use regia_net::replication::ServerReplication;
use regia_net::runtime::NetNode;
use regia_net::send::{MAX_CLIENT_RATE, MIN_CLIENT_RATE};
use regia_net::{NetId, NetSettings};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn server(world: &World, method: &str) -> Result<(), RpcError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net runtime",
            "add regia_net::NetPlugin (or call regia_net::runtime::ensure_resources) and set a NetMode",
        ));
    }
    if world
        .get_resource::<NetMode>()
        .copied()
        .unwrap_or_default()
        .is_server()
    {
        Ok(())
    } else {
        Err(f0002(
            "net_not_server",
            format!("{method} runs on the server; this process is a client"),
            "call it on the server, the listen host or an offline process",
        ))
    }
}

fn interest_off() -> RpcError {
    f0002(
        "interest_off",
        "the server has no relevance pipeline (every replicated entity is relevant to everyone)",
        "call regia_net::install_net_interest on the server world",
    )
}

/// Refuses a dev-build-only method in a shipping build (`allowed` is the build's answer, a
/// parameter so a test sees the refusal). Spec 72 marks `net.set_client_rate` and
/// `net.grant_relevance` as server play methods in development builds.
fn dev_gate(method: &str, allowed: bool) -> Result<(), RpcError> {
    if allowed {
        return Ok(());
    }
    Err(f0002(
        "net_dev_only",
        format!("{method} is available in development builds only"),
        "run a development build of the server",
    ))
}

fn peer(world: &World, params: &Value, required: bool) -> Result<Option<PeerId>, RpcError> {
    let p = match params.get("peer") {
        None | Some(Value::Null) if !required => return Ok(None),
        None | Some(Value::Null) => return Err(RpcError::invalid_params("`peer` (peer id) is required")),
        Some(v) => v
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .map(PeerId)
            .ok_or_else(|| RpcError::invalid_params("`peer` must be a peer id (an integer)"))?,
    };
    let known = world.get_resource::<NetNode>().is_some_and(|n| n.has_peer(p));
    if !known {
        return Err(f0002(
            "peer_unknown",
            format!("no connected peer has id {}", p.0),
            "net.status lists the connected peers",
        ));
    }
    Ok(Some(p))
}

fn entity(world: &World, params: &Value) -> Result<(Entity, NetId), RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let e = Entity::try_from_bits(bits)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or_else(|| {
            f0002(
                "entity_not_found",
                format!("entity {bits} does not exist"),
                "use net.replicated or scene.query_entities",
            )
        })?;
    let id = world.get::<NetId>(e).copied().ok_or_else(|| {
        f0002(
            "entity_not_replicated",
            format!("entity {bits} is not replicated"),
            "insert regia_net::Replicated on the server; net.replicated lists replicated entities",
        )
    })?;
    Ok((e, id))
}

fn tick(world: &World) -> NetTick {
    world.get_resource::<NetNode>().map_or(NetTick(0), |n| n.tick)
}

/// `net.relevance`: the entities relevant (or lingering) for a peer, with their priority
/// accumulator, last-sent and baseline ticks.
pub(crate) fn relevance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server(world, "net.relevance")?;
    let peer = peer(world, params, true)?.unwrap_or(PeerId::SERVER);
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(1000) as usize;
    let rep = world.get_resource::<ServerReplication>();
    let interest = world.get_resource::<NetInterest>();
    let mut rows = Vec::new();
    match interest.and_then(|i| i.peers.get(&peer)) {
        Some(pi) => {
            for (id, st) in pi.entity_states() {
                if rows.len() >= limit {
                    break;
                }
                let send = pi.send_state(id).unwrap_or_default();
                let copy = rep.and_then(|r| r.copy_ticks(peer, id));
                let e = rep.and_then(|r| r.entity_of(id));
                rows.push(json!({
                    "entity": e.map(Entity::to_bits),
                    "net_id": id.0,
                    "state": if st.linger_left.is_some() { "lingering" } else { "relevant" },
                    "acc": send.acc,
                    "last_sent_tick": copy.map(|c| c.0.0),
                    "baseline_tick": copy.and_then(|c| c.1).map(|t| t.0),
                }));
            }
        }
        None => {
            // No pipeline: every held copy is relevant.
            if let Some(r) = rep {
                for (id, e, _, _) in r.entities() {
                    if rows.len() >= limit {
                        break;
                    }
                    let Some((sent, base)) = r.copy_ticks(peer, id) else {
                        continue;
                    };
                    rows.push(json!({
                        "entity": e.to_bits(),
                        "net_id": id.0,
                        "state": "relevant",
                        "acc": 0.0,
                        "last_sent_tick": sent.0,
                        "baseline_tick": base.map(|t| t.0),
                    }));
                }
            }
        }
    }
    Ok(json!({ "peer": peer.0, "pipeline": interest.is_some(), "entities": rows }))
}

/// `net.why_not_relevant`: evaluates the § 5.1 pipeline for one (peer, entity) and names the
/// deciding stage.
pub(crate) fn why_not_relevant(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server(world, "net.why_not_relevant")?;
    let peer = peer(world, params, true)?.unwrap_or(PeerId::SERVER);
    let (_, id) = entity(world, params)?;
    let Some(interest) = world.get_resource::<NetInterest>() else {
        return Ok(json!({
            "relevant": true,
            "stage": "sent_to_all",
            "detail": "no relevance pipeline installed: every replicated entity is relevant",
            "lingering": false,
        }));
    };
    match interest.why(world, peer, id) {
        Some(v) => Ok(json!({
            "relevant": v.relevant,
            "stage": v.stage.as_str(),
            "detail": v.detail,
            "distance_m": v.distance,
            "lingering": interest.peers.get(&peer).is_some_and(|p| p.is_lingering(id)),
        })),
        None => Err(f0002(
            "entity_not_replicated",
            format!("net id {} is not tracked by the relevance pipeline yet", id.0),
            "wait one tick after the entity becomes replicated",
        )),
    }
}

/// `net.bandwidth`: bytes per second over the last 5 s per component or per entity (top 32),
/// with the budget and its use, for one peer or every peer.
pub(crate) fn bandwidth(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    server(world, "net.bandwidth")?;
    let only = peer(world, params, false)?;
    let by = crate::params::opt_str(params, "by")?.unwrap_or("component");
    if by != "component" && by != "entity" {
        return Err(RpcError::invalid_params(
            "`by` must be \"component\" or \"entity\"",
        ));
    }
    let project_rate = world
        .get_resource::<NetSettings>()
        .map_or(49_152, |s| s.replication.client_rate_bytes);
    // The same rate `SendConfig::from_world` stores. A fixed 60 Hz clock rolls a faster
    // server's 5 s window empty (spec 72: bytes per second over the last 5 s).
    let hz = regia_net::runtime::tick_hz(world);
    let hz = if hz.is_finite() && hz > 0.0 {
        hz
    } else {
        regia_net::send::DEFAULT_TICK_HZ
    };
    let second = (f64::from(tick(world).0) / hz) as u64;
    let Some(interest) = world.get_resource::<NetInterest>() else {
        return Ok(json!({ "pipeline": false, "peers": [] }));
    };
    let mut peers = Vec::new();
    for (p, pi) in &interest.peers {
        if only.is_some_and(|o| o != *p) {
            continue;
        }
        let bw = &pi.send.bandwidth;
        let mut keys: Vec<(String, f64)> = if by == "component" {
            bw.by_component
                .iter()
                .map(|(k, w)| {
                    (
                        k.rsplit("::").next().unwrap_or(k).to_string(),
                        w.per_second(second),
                    )
                })
                .collect()
        } else {
            bw.by_entity
                .iter()
                .map(|(k, w)| (k.0.to_string(), w.per_second(second)))
                .collect()
        };
        keys.retain(|(_, v)| *v > 0.0);
        keys.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        keys.truncate(32);
        let keys: serde_json::Map<String, Value> = keys.into_iter().map(|(k, v)| (k, json!(v))).collect();
        let c = pi.send.counters;
        peers.push(json!({
            "peer": p.0,
            "bytes_per_second": bw.total.per_second(second),
            "rate": pi.send.rate(project_rate),
            "declared_rate": pi.send.session_rate,
            "budget": c.budget,
            "budget_used": c.budget_used,
            "relevant": c.relevant,
            "spawns_pending": c.spawns_pending,
            "deferred": c.deferred,
            "starved": c.starved,
            "resyncs": c.resyncs,
            "tier_held": c.tier_held,
            "tier_sent": { "near": c.tier_sent[0], "mid": c.tier_sent[1], "far": c.tier_sent[2] },
            "by": by,
            "keys": keys,
        }));
    }
    let ic = interest.counters;
    let bands: Vec<f32> = interest.grid.band_radii().collect();
    Ok(json!({
        "pipeline": true,
        "peers": peers,
        "interest": {
            "evaluations": ic.evaluations,
            "grid_marked": ic.grid_marked,
            "held_kept": ic.held_kept,
            "band_radii_m": bands,
        },
    }))
}

/// `net.set_client_rate`: overrides one peer's rate (dev builds, spec 72).
pub(crate) fn set_client_rate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    set_client_rate_gated(world, params, cfg!(debug_assertions))
}

fn set_client_rate_gated(world: &mut World, params: &Value, allowed: bool) -> Result<Value, RpcError> {
    dev_gate("net.set_client_rate", allowed)?;
    server(world, "net.set_client_rate")?;
    let peer = peer(world, params, true)?.unwrap_or(PeerId::SERVER);
    let rate = params
        .get("bytes_per_second")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`bytes_per_second` (integer) is required"))?;
    if !(u64::from(MIN_CLIENT_RATE)..=u64::from(MAX_CLIENT_RATE)).contains(&rate) {
        return Err(f0002(
            "rate_out_of_range",
            format!("{rate} B/s is outside {MIN_CLIENT_RATE}..={MAX_CLIENT_RATE}"),
            "use 4 KiB/s to 10 MiB/s",
        ));
    }
    let rate = rate as u32;
    let mut interest = world.get_resource_mut::<NetInterest>().ok_or_else(interest_off)?;
    let pi = interest.peers.entry(peer).or_default();
    pi.send.rate_override = Some(rate);
    Ok(json!({ "rate": rate }))
}

/// `net.profiles`: the loaded profiles with their users count (and the ones that failed to load).
/// With `name`, only that profile; a profile that failed to load answers `profile_invalid`.
pub(crate) fn profiles(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net runtime",
            "add regia_net::NetPlugin and set a NetMode",
        ));
    }
    let profiles = world.get_resource::<NetProfiles>().cloned().unwrap_or_default();
    let users = world
        .get_resource::<NetInterest>()
        .map(NetInterest::profile_users)
        .unwrap_or_default();
    let name = crate::params::opt_str(params, "name")?.map(regia_net::profile::profile_key);
    if let Some(n) = name
        && profiles.get(n).is_none()
    {
        let why = profiles
            .failed
            .get(n)
            .map_or_else(|| "not loaded".to_string(), ToString::to_string);
        return Err(f0002(
            "profile_invalid",
            format!("profile `{n}`: {why}"),
            "fix the field named in the message; net.profiles lists the loaded profiles",
        ));
    }
    let rows: Vec<Value> = profiles
        .iter()
        .filter(|(k, _)| name.is_none_or(|n| n == *k))
        .map(|(k, p)| {
            let relevance = match p.relevance {
                RelevanceShape::Spatial { radius, hysteresis } => {
                    json!({ "spatial": { "radius": radius, "hysteresis": hysteresis } })
                }
                RelevanceShape::World => json!("world"),
            };
            json!({
                "name": k,
                "users": users.get(k).copied().unwrap_or(0),
                "default": k == profiles.default_key(),
                "priority": p.priority,
                "relevance": relevance,
                "sent_to_all": p.sent_to_all,
                "owner_only": p.owner_only,
                "group_required": p.group_required,
                "follow_parent": p.follow_parent,
                "custom_filter": p.custom_filter,
                "min_interval_ticks": p.min_interval_ticks,
                "max_interval_ticks": p.max_interval_ticks,
                "linger_ticks": p.linger_ticks,
                "owner_boost": p.owner_boost,
                "view_falloff": p.view_falloff,
                "position_step_m": p.position_step_m,
                "interpolation": p.interpolation.as_str(),
                "snap_distance_m": p.snap_distance_m,
                "max_extrapolate_ms": p.max_extrapolate_ms,
                "appear_immediately": p.appear_immediately,
                "rate_tiers": p.rate_tiers.map(|t| json!({
                    "near_m": t.near_m,
                    "mid_m": t.mid_m,
                    "mid_every": t.mid_every,
                    "far_hz": t.far_hz,
                })),
                // Spec 120 § 8.2, ADR-8522 (`null`: off).
                "extrapolation_gate_m": p.extrapolation_gate_m,
            })
        })
        .collect();
    let failed: Vec<Value> = profiles
        .failed
        .iter()
        .map(|(k, e)| json!({ "name": k, "error": e.to_string() }))
        .collect();
    Ok(json!({ "profiles": rows, "failed": failed }))
}

/// `net.grant_relevance`: makes an entity relevant to a peer or group for a number of ticks
/// (dev builds, spec 72).
pub(crate) fn grant_relevance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    grant_relevance_gated(world, params, cfg!(debug_assertions))
}

fn grant_relevance_gated(world: &mut World, params: &Value, allowed: bool) -> Result<Value, RpcError> {
    dev_gate("net.grant_relevance", allowed)?;
    server(world, "net.grant_relevance")?;
    let (e, _) = entity(world, params)?;
    let ticks = crate::params::opt_u64(params, "ticks")?
        .and_then(|t| u32::try_from(t).ok())
        .filter(|t| *t > 0)
        .ok_or_else(|| RpcError::invalid_params("`ticks` (a positive integer) is required"))?;
    let target = match (params.get("peer"), crate::params::opt_str(params, "group")?) {
        (Some(p), None) if !p.is_null() => {
            GrantTarget::Peer(peer(world, params, true)?.unwrap_or(PeerId::SERVER))
        }
        (None | Some(Value::Null), Some(g)) => GrantTarget::Group(g.to_string()),
        _ => {
            return Err(RpcError::invalid_params(
                "exactly one of `peer` or `group` is required",
            ));
        }
    };
    if world.get_resource::<NetInterest>().is_none() {
        return Err(interest_off());
    }
    let until = NetTick(tick(world).0.saturating_add(ticks));
    regia_net::net_grant_relevance(world, e, target, until);
    Ok(json!({ "until_tick": until.0 }))
}

/// `net.relevance_gizmos {visible?, peer?, limit?}`: shows or hides the `net_relevance` gizmo
/// group (spec 72 § 10, ADR-2665 (5)), chooses the connection it draws, and returns that
/// connection's view: each replicated entity's state (`relevant`, `lingering`, `filtered_out`),
/// the viewpoints, the grid cell and the default radius. Off by default; no parameter changes
/// nothing (a read).
pub(crate) fn relevance_gizmos(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_net::replication::relevance_view::{
        NetRelevanceView, RelevanceMarkState, relevance_snapshot, view_peer,
    };
    use regia_render::gizmos::GizmoConfigStore;
    server(world, "net.relevance_gizmos")?;
    let visible = match params.get("visible") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`visible` must be a boolean"))?,
        ),
    };
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(1000) as usize;
    let chosen = peer(world, params, false)?;
    // The group exists where a renderer and the bridge are (a listen host, a server with gizmos).
    let group = "net_relevance";
    let shown = match world.get_resource_mut::<GizmoConfigStore>() {
        Some(mut store) => match store.config_by_name_mut(group) {
            Some(config) => {
                if let Some(on) = visible {
                    config.enabled = on;
                }
                Some(config.enabled)
            }
            None => None,
        },
        None => None,
    };
    if visible.is_some() && shown.is_none() {
        return Err(f0002(
            "net_relevance_gizmos_unavailable",
            "this app draws no gizmos (a dedicated server, or no renderer); the view is still readable",
            "call net.relevance_gizmos without `visible` for the rows, or run a listen host with a window",
        ));
    }
    if let Some(p) = chosen {
        world.get_resource_or_insert_with(NetRelevanceView::default).peer = Some(p);
    }
    let Some(peer) = view_peer(world) else {
        return Ok(json!({ "visible": shown, "peer": null, "entities": [] }));
    };
    let Some(snap) = relevance_snapshot(world, peer) else {
        return Ok(json!({ "visible": shown, "peer": peer.0, "entities": [] }));
    };
    let entities: Vec<Value> = snap
        .marks
        .iter()
        .take(limit)
        .map(|m| {
            json!({
                "entity": m.entity.to_bits(),
                "net_id": m.net_id.0,
                "state": m.state.as_str(),
                "asleep": m.asleep,
            })
        })
        .collect();
    let viewpoints: Vec<Value> = snap
        .viewpoints
        .iter()
        .map(|(at, fwd)| json!({ "position": [at.x, at.y, at.z], "forward": [fwd.x, fwd.y, fwd.z] }))
        .collect();
    Ok(json!({
        "visible": shown,
        "peer": peer.0,
        "pipeline": snap.pipeline,
        "cell_m": snap.cell_m,
        "radius_m": snap.radius_m,
        "viewpoints": viewpoints,
        "counts": {
            "relevant": snap.count(RelevanceMarkState::Relevant),
            "lingering": snap.count(RelevanceMarkState::Lingering),
            "filtered_out": snap.count(RelevanceMarkState::FilteredOut),
        },
        "entities": entities,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_ecs::prelude::Component;
    use regia_net::model::{NetOwner, NetPlayerId};
    use regia_net::replication::register_replicated;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetProfile, NetProfileRef, NetViewpoint, ReplicateOptions, Replicated};
    use regia_scene::transform::Transform;

    #[derive(Component, Clone, Debug, PartialEq, Default, regia_net::Replicate)]
    struct Hp(u32);

    fn setup(world: &mut World) {
        register_replicated::<Hp>(world, ReplicateOptions::default());
        if world.resource::<NetMode>().is_server() {
            regia_net::install_net_interest(world);
            let _ = world
                .resource_mut::<NetProfiles>()
                .load_ron("broken", "(priority: -2.0)");
            let near = NetProfile {
                relevance: RelevanceShape::Spatial {
                    radius: 10.0,
                    hysteresis: 0.0,
                },
                linger_ticks: 0,
                ..NetProfile::default()
            };
            let _ = world.resource_mut::<NetProfiles>().insert("near", near);
        }
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

    fn at(x: f32) -> Transform {
        Transform {
            translation: Vec3::new(x, 0.0, 0.0),
            ..Transform::default()
        }
    }

    /// Spec 72 § 13 test 13: every method round-trips; `why_not_relevant` names the stage; each
    /// error reason is produced.
    #[test]
    fn net_relevance_methods_round_trip_and_every_reason() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
        let a = hub.add_client();
        let peer = hub.clients[a].peer;
        hub.server.world.spawn((
            NetViewpoint {
                player: NetPlayerId { peer, local: 0 },
            },
            at(0.0),
        ));
        let near = hub
            .server
            .world
            .spawn((rep(), Hp(1), at(2.0), NetProfileRef("near".into())))
            .id();
        let far = hub
            .server
            .world
            .spawn((rep(), Hp(2), at(200.0), NetProfileRef("near".into())))
            .id();
        let owned = hub
            .server
            .world
            .spawn((
                rep(),
                Hp(3),
                at(500.0),
                NetProfileRef("near".into()),
                NetOwner(peer),
            ))
            .id();
        let local = hub.server.world.spawn(Hp(9)).id();
        hub.run(4);
        let sv = &mut hub.server.world;
        let p = json!(peer.0);

        let rel = relevance(sv, &json!({ "peer": p })).expect("relevance");
        assert_eq!(rel["pipeline"], true);
        let rows = rel["entities"].as_array().cloned().unwrap_or_default();
        assert_eq!(rows.len(), 2, "near and owned: {rows:?}");
        assert!(rows.iter().all(|r| r["state"] == "relevant"));
        let why = |sv: &mut World, e: Entity| {
            why_not_relevant(sv, &json!({ "peer": p, "entity": e.to_bits() })).expect("why")
        };
        assert_eq!(why(sv, near)["stage"], "spatial");
        assert_eq!(why(sv, near)["relevant"], true);
        assert_eq!(why(sv, far)["stage"], "spatial");
        assert_eq!(why(sv, far)["relevant"], false);
        assert_eq!(why(sv, owned)["stage"], "owner");
        let bw = bandwidth(sv, &json!({ "peer": p, "by": "component" })).expect("bandwidth");
        assert_eq!(bw["peers"][0]["peer"], peer.0);
        assert!(
            bw["peers"][0]["bytes_per_second"]
                .as_f64()
                .is_some_and(|b| b > 0.0)
        );
        assert!(
            bw["peers"][0]["keys"]
                .as_object()
                .is_some_and(|k| k.contains_key("Hp"))
        );
        let bw = bandwidth(sv, &json!({ "by": "entity" })).expect("bandwidth by entity");
        assert_eq!(bw["peers"].as_array().map(Vec::len), Some(1));
        let rate = set_client_rate(sv, &json!({ "peer": p, "bytes_per_second": 8192 })).expect("rate");
        assert_eq!(rate["rate"], 8192);
        let profs = profiles(sv, &json!({})).expect("profiles");
        let near_row = profs["profiles"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["name"] == "near").cloned())
            .expect("near profile");
        assert_eq!(near_row["users"], 3);
        // ADR-8522: the gate is listed, `null` when off (the default).
        assert!(near_row["extrapolation_gate_m"].is_null(), "{near_row}");
        assert_eq!(profs["failed"][0]["name"], "broken");
        let g =
            grant_relevance(sv, &json!({ "entity": far.to_bits(), "peer": p, "ticks": 30 })).expect("grant");
        let now = sv.resource::<NetNode>().tick.0;
        assert_eq!(g["until_tick"], now + 30);
        hub.run(3);
        let sv = &mut hub.server.world;
        assert_eq!(why(sv, far)["stage"], "grant");
        let g = grant_relevance(
            sv,
            &json!({ "entity": far.to_bits(), "group": "red", "ticks": 5 }),
        )
        .expect("group");
        assert!(g["until_tick"].as_u64().is_some());

        // Errors.
        let err = relevance(sv, &json!({ "peer": 99 })).expect_err("peer");
        assert_eq!(reason(&err).as_deref(), Some("peer_unknown"));
        let err = why_not_relevant(sv, &json!({ "peer": p, "entity": local.to_bits() })).expect_err("local");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_replicated"));
        let gone = sv.spawn_empty().id();
        assert!(sv.despawn(gone));
        let err = grant_relevance(sv, &json!({ "entity": gone.to_bits(), "peer": p, "ticks": 3 }))
            .expect_err("gone");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_found"));
        let err = set_client_rate(sv, &json!({ "peer": p, "bytes_per_second": 100 })).expect_err("rate");
        assert_eq!(reason(&err).as_deref(), Some("rate_out_of_range"));
        let err = profiles(sv, &json!({ "name": "broken" })).expect_err("broken");
        assert_eq!(reason(&err).as_deref(), Some("profile_invalid"));
        sv.remove_resource::<NetInterest>();
        let err = set_client_rate(sv, &json!({ "peer": p, "bytes_per_second": 8192 })).expect_err("off");
        assert_eq!(reason(&err).as_deref(), Some("interest_off"));
        let cw = &mut hub.clients[a].world;
        let err = relevance(cw, &json!({ "peer": 1 })).expect_err("client");
        assert_eq!(reason(&err).as_deref(), Some("net_not_server"));
        let mut bare = World::new();
        let err = bandwidth(&mut bare, &json!({})).expect_err("bare");
        assert_eq!(reason(&err).as_deref(), Some("net_not_running"));
        // Default rate 49152 B/s, send every 2 ticks, 60 Hz: send_hz is 30, so one send
        // tick's budget before carry is 1638 bytes (spec 72 § 7). `net.bandwidth` returns
        // the last send's counter, which also includes at most one tick of carry.
        let bare_budget = regia_net::send::tick_budget(49_152, 30.0, 1.0, 0.0, 24);
        assert_eq!(bare_budget.bytes as u32, 1638);
    }

    /// Spec 72: the 5 s window uses the world's tick rate, the same clock the sender buckets with.
    /// At 120 Hz, tick 600 is second 5. Dividing by 60 would read second 10 and roll the window empty.
    #[test]
    fn bandwidth_window_follows_the_world_tick_rate() {
        use regia_net::relevance::PeerInterest;
        use regia_net::runtime::NetNode;

        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        let mut node = NetNode::default();
        node.tick = NetTick(600);
        world.insert_resource(node);
        let mut fixed = regia_app::FixedTime::default();
        fixed.hz = 120.0;
        world.insert_resource(fixed);
        regia_net::install_net_interest(&mut world);
        {
            let mut interest = world.resource_mut::<NetInterest>();
            let mut peer = PeerInterest::default();
            peer.send.bandwidth.total.add(5, 5_000);
            interest.peers.insert(PeerId(7), peer);
        }
        let bw = bandwidth(&mut world, &json!({ "by": "component" })).expect("bandwidth");
        assert_eq!(bw["pipeline"], true);
        assert_eq!(bw["peers"][0]["peer"], 7);
        assert_eq!(bw["peers"][0]["bytes_per_second"].as_f64(), Some(1_000.0));
    }

    /// Spec 72: `net.set_client_rate` and `net.grant_relevance` refuse a shipping build
    /// before they change the connection.
    #[test]
    fn rate_and_grant_are_dev_build_only() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        world.insert_resource(regia_net::runtime::NetNode::default());
        regia_net::install_net_interest(&mut world);
        let err = set_client_rate_gated(
            &mut world,
            &json!({ "peer": 1, "bytes_per_second": 65536 }),
            false,
        )
        .expect_err("shipping");
        assert_eq!(reason(&err).as_deref(), Some("net_dev_only"));
        assert!(
            world
                .resource::<NetInterest>()
                .peers
                .values()
                .all(|p| p.send.rate_override.is_none())
        );
        let err = grant_relevance_gated(
            &mut world,
            &json!({ "entity": 1, "peer": 1, "ticks": 300 }),
            false,
        )
        .expect_err("shipping");
        assert_eq!(reason(&err).as_deref(), Some("net_dev_only"));
        assert!(world.resource::<NetInterest>().grants.is_empty());
    }

    #[test]
    fn bandwidth_uses_the_recorders_fixed_tick_clock() {
        for hz in [120.0, 90.0, 60.0, 30.0] {
            let mut world = World::new();
            world.insert_resource(NetMode::DedicatedServer);
            let mut fixed = FixedTime::default();
            fixed.hz = hz;
            world.insert_resource(fixed);
            regia_net::runtime::ensure_resources(&mut world);
            let peer = PeerId(1);
            regia_net::runtime::server_add_peer(&mut world, peer);
            regia_net::install_net_interest(&mut world);
            let tick = (hz * 10.0) as u32;
            world.resource_mut::<NetNode>().tick = NetTick(tick);
            let second = (f64::from(tick) / regia_net::runtime::tick_hz(&world)) as u64;
            {
                let mut interest = world.resource_mut::<NetInterest>();
                let bw = &mut interest.peers.entry(peer).or_default().send.bandwidth;
                bw.total.add(second, 500);
                bw.by_component.entry("test::Hp").or_default().add(second, 300);
                bw.by_entity.entry(NetId(7)).or_default().add(second, 200);
            }
            for (by, key, rate) in [("component", "Hp", 60.0), ("entity", "7", 40.0)] {
                let out = bandwidth(&mut world, &json!({ "peer": peer.0, "by": by })).expect("bandwidth");
                assert_eq!(out["peers"][0]["bytes_per_second"], 100.0, "{hz} Hz: {out}");
                assert_eq!(out["peers"][0]["keys"][key], rate, "{hz} Hz: {out}");
            }
            world.resource_mut::<NetNode>().tick = NetTick(tick * 2);
            let expired = bandwidth(&mut world, &json!({ "peer": peer.0 })).expect("expired");
            assert_eq!(expired["peers"][0]["bytes_per_second"], 0.0);
            assert_eq!(expired["peers"][0]["keys"], json!({}));
        }
    }

    /// ADR-2665 (5): `net.relevance_gizmos` reads each replicated entity's state for the chosen
    /// connection (relevant, lingering, filtered out; asleep flagged), chooses the connection,
    /// and refuses `visible` where no gizmo group exists (here: a world without a renderer).
    #[test]
    fn relevance_gizmos_reports_the_view_and_needs_a_renderer_to_show() {
        use regia_net::NetAsleep;
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 6, setup);
        let a = hub.add_client();
        let b = hub.add_client();
        let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
        hub.server.world.spawn((
            NetViewpoint {
                player: NetPlayerId { peer: pa, local: 0 },
            },
            at(0.0),
        ));
        hub.server.world.spawn((
            NetViewpoint {
                player: NetPlayerId { peer: pb, local: 0 },
            },
            at(200.0),
        ));
        let near = hub
            .server
            .world
            .spawn((rep(), Hp(1), at(2.0), NetProfileRef("near".into())))
            .id();
        let far = hub
            .server
            .world
            .spawn((rep(), Hp(2), at(201.0), NetProfileRef("near".into()), NetAsleep))
            .id();
        hub.run(4);
        let sv = &mut hub.server.world;
        let row = |v: &Value, e: Entity| -> Value {
            v["entities"]
                .as_array()
                .and_then(|l| l.iter().find(|r| r["entity"] == json!(e.to_bits())).cloned())
                .unwrap_or(Value::Null)
        };
        // Default: the lowest peer id.
        let first = pa.min(pb);
        let v = relevance_gizmos(sv, &json!({})).expect("read");
        assert_eq!(v["peer"], json!(first.0));
        assert_eq!(v["visible"], Value::Null, "no gizmo store in this world");
        assert_eq!(v["pipeline"], true);
        // Peer a sees `near`, not `far`; peer b the other way round.
        let v = relevance_gizmos(sv, &json!({ "peer": pa.0 })).expect("peer a");
        assert_eq!(v["peer"], json!(pa.0));
        assert_eq!(row(&v, near)["state"], "relevant", "{v}");
        assert_eq!(row(&v, far)["state"], "filtered_out", "{v}");
        assert_eq!(row(&v, far)["asleep"], true);
        assert_eq!(v["counts"]["relevant"], 1);
        assert_eq!(v["counts"]["filtered_out"], 1);
        assert_eq!(v["viewpoints"].as_array().map(Vec::len), Some(1));
        let v = relevance_gizmos(sv, &json!({ "peer": pb.0 })).expect("peer b");
        assert_eq!(row(&v, near)["state"], "filtered_out");
        assert_eq!(row(&v, far)["state"], "relevant");
        // The choice sticks for the drawing.
        let v = relevance_gizmos(sv, &json!({})).expect("read");
        assert_eq!(v["peer"], json!(pb.0));
        // Showing it needs the gizmo group.
        let err = relevance_gizmos(sv, &json!({ "visible": true })).expect_err("no renderer");
        assert_eq!(reason(&err).as_deref(), Some("net_relevance_gizmos_unavailable"));
        assert!(relevance_gizmos(sv, &json!({ "visible": 3 })).is_err());
        let err = relevance_gizmos(sv, &json!({ "peer": 99 })).expect_err("peer");
        assert_eq!(reason(&err).as_deref(), Some("peer_unknown"));
        let err = relevance_gizmos(&mut hub.clients[a].world, &json!({})).expect_err("client");
        assert_eq!(reason(&err).as_deref(), Some("net_not_server"));
    }

    /// `net.bandwidth` reports the distance rate tiers' counts per peer and the relevance
    /// update's work (spec 120 § 8.3-8.4, ADR-8031).
    #[test]
    fn bandwidth_reports_tiers_and_interest_work() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        regia_net::runtime::ensure_resources(&mut world);
        regia_net::runtime::server_add_peer(&mut world, PeerId(1));
        let mut interest = NetInterest::default();
        let c = &mut interest.peers.entry(PeerId(1)).or_default().send.counters;
        c.tier_held = 7;
        c.tier_sent = [5, 2, 1];
        interest.counters.evaluations = 40;
        interest.counters.grid_marked = 90;
        interest.counters.held_kept = 300;
        world.insert_resource(interest);
        let bw = bandwidth(&mut world, &json!({ "peer": 1 })).expect("bandwidth");
        assert_eq!(bw["peers"][0]["tier_held"], json!(7));
        assert_eq!(
            bw["peers"][0]["tier_sent"],
            json!({ "near": 5, "mid": 2, "far": 1 })
        );
        assert_eq!(
            bw["interest"],
            json!({ "evaluations": 40, "grid_marked": 90, "held_kept": 300, "band_radii_m": [] })
        );
    }

    /// Bytes recorded on the world's tick rate stay visible at tick 540 when that rate is 120 Hz.
    /// Dividing the same tick by 60 asks for second 9 and the 5 s window comes back empty.
    #[test]
    fn bandwidth_follows_the_world_tick_rate() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        let mut time = FixedTime::default();
        time.hz = 120.0;
        world.insert_resource(time);
        regia_net::runtime::ensure_resources(&mut world);
        regia_net::runtime::server_add_peer(&mut world, PeerId(1));
        world.resource_mut::<NetNode>().tick = NetTick(540);
        let mut interest = NetInterest::default();
        interest
            .peers
            .entry(PeerId(1))
            .or_default()
            .send
            .bandwidth
            .total
            .add(4, 5_000);
        world.insert_resource(interest);
        let bw = bandwidth(&mut world, &json!({ "peer": 1 })).expect("bandwidth");
        let bps = bw["peers"][0]["bytes_per_second"].as_f64().expect("number");
        assert!(
            (bps - 1_000.0).abs() < 1e-6,
            "5_000 bytes in one of five seconds is 1_000 B/s, got {bps}"
        );
    }

    /// Spec 72 § 12: `net.set_client_rate` refuses rates below 4 KiB/s or above 10 MiB/s with
    /// `rate_out_of_range`, accepts both bounds, and an accepted rate sets the connection's budget.
    #[test]
    fn client_rate_bounds_and_the_budget_follows_the_rate() {
        assert_eq!(MIN_CLIENT_RATE, 4 * 1024);
        assert_eq!(MAX_CLIENT_RATE, 10 * 1024 * 1024);
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 9, setup);
        let a = hub.add_client();
        let peer = hub.clients[a].peer;
        hub.server.world.spawn((rep(), Hp(1), at(0.0)));
        hub.run(4);
        let set = |world: &mut World, rate: u32| {
            set_client_rate_gated(world, &json!({ "peer": peer.0, "bytes_per_second": rate }), true)
        };
        for bad in [MIN_CLIENT_RATE - 1, MAX_CLIENT_RATE + 1] {
            let err = set(&mut hub.server.world, bad).expect_err("out of range");
            assert_eq!(reason(&err).as_deref(), Some("rate_out_of_range"), "{bad}");
        }
        let budget = |hub: &LoopbackHub| {
            hub.server.world.resource::<NetInterest>().peers[&peer]
                .send
                .counters
                .budget
        };
        // The hub sends every tick at 60 Hz; unused budget carries over at most one send tick.
        for good in [MIN_CLIENT_RATE, MAX_CLIENT_RATE] {
            assert_eq!(set(&mut hub.server.world, good).expect("in range")["rate"], good);
            hub.run(4);
            let per_tick = f64::from(good) / 60.0;
            let b = f64::from(budget(&hub));
            assert!(
                b >= per_tick - 1.0 && b <= 2.0 * per_tick + 1.0,
                "{good} B/s: budget {b} per tick, expected {per_tick}..{}",
                2.0 * per_tick
            );
        }
    }
}
