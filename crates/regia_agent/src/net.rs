// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for `regia_net` (`book/agents/net.md`, ADR-1600): `net.status`,
//! `net.peers`, `net.conditioner_set`, `net.rollback` (`book/agents/net-rollback.md`, ADR-7953)
//! and, for transport encryption, `net.crypto` (`book/agents/net-crypto.md`, ADR-1901); for
//! lockstep matches, `net.lockstep` (`book/agents/net-lockstep.md`, ADR-8146); for connection
//! grades, `net.quality` (`book/agents/net-quality.md`, ADR-8182).
//!
//! The handlers read and write the `regia_net::NetAgentState` resource that a game's servers and
//! clients publish into each frame (`NetServer::publish` / `NetClient::publish`), so this crate
//! never needs the game's payload types. Without the resource the reads answer empty lists and
//! `net.conditioner_set` answers `net_no_endpoint`.

use regia_app::prelude::*;
use regia_net::{CONDITIONER_PROFILES, ConditionerConfig, NetAgentState};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError, UNSUPPORTED};

fn endpoint_param(params: &Value) -> Result<Option<&str>, RpcError> {
    match params.get("endpoint") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(RpcError::invalid_params("`endpoint` must be a non-empty string")),
    }
}

fn unknown_endpoint(label: &str, state: Option<&NetAgentState>) -> RpcError {
    let known: Vec<&str> = state.map_or_else(Vec::new, |s| s.reports().keys().map(String::as_str).collect());
    RpcError::new(
        NOT_FOUND,
        "net_unknown_endpoint",
        format!("no network endpoint named `{label}`"),
        format!("published endpoints: {known:?} (net.status lists them)"),
    )
}

/// `net.status`: every published endpoint's status (role, state, addresses, tick, peer count,
/// conditioner and counters).
pub fn status(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let mut out = endpoint_status(world);
    // Spec 70 § 9: the net model's mode, local peer, tick, peers and players (ADR-1643).
    if let (Value::Object(map), Some(model)) = (&mut out, crate::net_model::status_fields(world)) {
        map.extend(model);
    }
    Ok(out)
}

fn endpoint_status(world: &World) -> Value {
    let Some(state) = world.get_resource::<NetAgentState>() else {
        return json!({ "bridge": false, "endpoints": [] });
    };
    let endpoints: Vec<Value> = state
        .reports()
        .iter()
        .map(|(label, report)| {
            let mut v = serde_json::to_value(&report.status).unwrap_or(Value::Null);
            if let Value::Object(map) = &mut v {
                map.insert("label".into(), json!(label));
            }
            v
        })
        .collect();
    json!({ "bridge": true, "endpoints": endpoints })
}

/// `net.peers`: per-peer health and link counters of one endpoint, or of all.
pub fn peers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let state = world.get_resource::<NetAgentState>();
    if let Some(label) = target
        && !state.is_some_and(|s| s.reports().contains_key(label))
    {
        return Err(unknown_endpoint(label, state));
    }
    let endpoints: Vec<Value> = state
        .map(|s| {
            s.reports()
                .iter()
                .filter(|(label, _)| target.is_none_or(|t| t == label.as_str()))
                .map(|(label, report)| {
                    json!({
                        "label": label,
                        "role": report.status.role,
                        "peers": serde_json::to_value(&report.peers).unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "endpoints": endpoints }))
}

/// `net.rejections` (KI-NT-025, ADR-10077): what each peer had refused, by lowercase refusal code
/// (`RejectKind::as_str`), peers that left included: per published server endpoint the
/// transport's `ServerEvent::Rejected` refusals, and on a server world the replication runtime's
/// (`NetNode::reject`). `endpoint` limits the endpoints, `peer` the rows. Read-only.
pub fn rejections(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let peer = match params.get("peer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().and_then(|p| u32::try_from(p).ok()).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "net_rejections_bad_peer",
                "`peer` must be a peer id (an integer 0..=4294967295)",
                "net.peers and net.rejections list the peer ids",
            )
        })?),
    };
    let state = world.get_resource::<NetAgentState>();
    if let Some(label) = target
        && !state.is_some_and(|s| s.reports().contains_key(label))
    {
        return Err(unknown_endpoint(label, state));
    }
    let keep = |r: &&regia_net::PeerRejections| peer.is_none_or(|p| p == r.peer);
    let endpoints: Vec<Value> = state
        .map(|s| {
            s.reports()
                .iter()
                .filter(|(label, _)| target.is_none_or(|t| t == label.as_str()))
                .map(|(label, report)| {
                    let rows: Vec<&regia_net::PeerRejections> =
                        report.rejections.iter().filter(keep).collect();
                    json!({
                        "label": label,
                        "role": report.status.role,
                        "peers": serde_json::to_value(rows).unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let server = world
        .get_resource::<regia_net::model::NetMode>()
        .is_some_and(|m| m.is_server());
    let runtime = world
        .get_resource::<regia_net::runtime::NetNode>()
        .filter(|_| server)
        .map(|node| {
            let (total, dropped) = node.rejection_counts();
            let rows = node.rejections_by_peer().rows();
            let rows: Vec<&regia_net::PeerRejections> = rows.iter().filter(keep).collect();
            json!({
                "total": total,
                "dropped": dropped,
                "peers": serde_json::to_value(rows).unwrap_or(Value::Null),
            })
        })
        .unwrap_or(Value::Null);
    Ok(json!({ "endpoints": endpoints, "runtime": runtime }))
}

/// `net.resume`: replication-game resume on this server (spec 120 § 10.3, ADR-8298): the dormant
/// peers and the resume, grace-end and held-message counters. Empty on a client or before any
/// peer went dormant.
pub fn resume(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let hook = world.contains_resource::<regia_net::NetGraceHook>();
    let Some(st) = world.get_resource::<regia_net::NetResumeState>() else {
        return Ok(json!({
            "hook": hook,
            "dormant": [],
            "went_dormant": 0,
            "resumed": 0,
            "takeovers": 0,
            "expired": 0,
            "hook_calls": 0,
            "kept": 0,
            "despawned_entities": 0,
            "messages_kept": 0,
            "messages_expired": 0,
            "messages_overflowed": 0,
            "last_resume": null,
        }));
    };
    let dormant: Vec<Value> = st
        .dormant
        .iter()
        .map(|(peer, d)| json!({ "peer": peer.0, "since_tick": d.since_tick }))
        .collect();
    Ok(json!({
        "hook": hook,
        "dormant": dormant,
        "went_dormant": st.went_dormant,
        "resumed": st.resumed,
        "takeovers": st.takeovers,
        "expired": st.expired,
        "hook_calls": st.hook_calls,
        "kept": st.kept,
        "despawned_entities": st.despawned_entities,
        "messages_kept": st.messages_kept,
        "messages_expired": st.messages_expired,
        "messages_overflowed": st.messages_overflowed,
        "last_resume": st.last_resume.map(|(peer, tick)| json!({ "peer": peer.0, "tick": tick })),
    }))
}

/// `net.quality`: connection quality (spec 120 § 13.1, ADR-8182). A server endpoint lists one row
/// per peer (`id`, `name`, `grade`, `interrupted`, `quality`); a client endpoint reports its link
/// to the server (`grade`, `interrupted`, `quality`, null before it connects). `peer` keeps one
/// server row and answers `peer_unknown` when no endpoint has it.
pub fn quality(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let peer = match params.get("peer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`peer` must be a peer id (an unsigned integer)"))?,
        ),
    };
    let state = world.get_resource::<NetAgentState>();
    if let Some(label) = target
        && !state.is_some_and(|s| s.reports().contains_key(label))
    {
        return Err(unknown_endpoint(label, state));
    }
    let mut found = false;
    let endpoints: Vec<Value> = state
        .map(|s| {
            s.reports()
                .iter()
                .filter(|(label, _)| target.is_none_or(|t| t == label.as_str()))
                .map(|(label, report)| {
                    let rows: Vec<Value> = report
                        .peers
                        .iter()
                        .filter(|p| peer.is_none_or(|id| id == p.id))
                        .map(|p| {
                            json!({
                                "id": p.id,
                                "name": p.name,
                                "grade": p.grade,
                                "interrupted": p.interrupted,
                                "quality": p.quality,
                            })
                        })
                        .collect();
                    found |= !rows.is_empty();
                    let link = report.status.quality;
                    json!({
                        "label": label,
                        "role": report.status.role,
                        "grade": link.map(|q| q.grade),
                        "interrupted": report.status.interrupted,
                        "quality": link,
                        "peers": rows,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(id) = peer
        && !found
    {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "peer_unknown",
            format!("no endpoint has a peer {id}"),
            "net.quality without `peer`, or net.peers, lists the connected peers",
        ));
    }
    Ok(json!({
        "limits": {
            "good": regia_net::quality::GOOD_LIMITS,
            "fair": regia_net::quality::FAIR_LIMITS,
        },
        "endpoints": endpoints,
    }))
}

/// `net.crypto`: each endpoint's encryption: mode, suite, server key, whether its links are sealed,
/// and the counters of what the record layer refused.
pub fn crypto(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let state = world.get_resource::<NetAgentState>();
    if let Some(label) = target
        && !state.is_some_and(|s| s.reports().contains_key(label))
    {
        return Err(unknown_endpoint(label, state));
    }
    let endpoints: Vec<Value> = state
        .map(|s| {
            s.reports()
                .iter()
                .filter(|(label, _)| target.is_none_or(|t| t == label.as_str()))
                .map(|(label, report)| {
                    let totals = report.status.totals;
                    let mut v = serde_json::to_value(&report.status.crypto).unwrap_or(Value::Null);
                    if let Value::Object(map) = &mut v {
                        map.insert("label".into(), json!(label));
                        map.insert("role".into(), json!(report.status.role));
                        map.insert(
                            "counters".into(),
                            json!({
                                "sealed_out": totals.sealed_out,
                                "opened_in": totals.opened_in,
                                "auth_failed": totals.auth_failed,
                                "replayed": totals.replayed,
                                "handshakes": totals.handshakes,
                                "handshakes_refused": totals.handshakes_refused,
                                "pre_proof_shed": totals.pre_proof_shed,
                            }),
                        );
                    }
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "endpoints": endpoints }))
}

/// `net.lockstep`: each published lockstep host's or seat's status (spec 120 § 13.3, ADR-8146):
/// the input delay and its target, raises and lowers, frame lag and input timing per seat, desync
/// verdicts and resyncs.
pub fn lockstep(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let state = world.get_resource::<NetAgentState>();
    if let Some(label) = target
        && !state.is_some_and(|s| s.lockstep().contains_key(label))
    {
        let known: Vec<&str> =
            state.map_or_else(Vec::new, |s| s.lockstep().keys().map(String::as_str).collect());
        return Err(RpcError::new(
            NOT_FOUND,
            "lockstep_unknown_endpoint",
            format!("no lockstep host or seat named `{label}`"),
            format!("published: {known:?} (net.lockstep without `endpoint` lists them)"),
        ));
    }
    let endpoints: Vec<Value> = state
        .map(|s| {
            s.lockstep()
                .iter()
                .filter(|(label, _)| target.is_none_or(|t| t == label.as_str()))
                .map(|(label, status)| {
                    let mut v = serde_json::to_value(status).unwrap_or(Value::Null);
                    if let Value::Object(map) = &mut v {
                        map.insert("label".into(), json!(label));
                    }
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "endpoints": endpoints }))
}

/// `net.migration`: each published host journal's or successor's status (spec 120 § 10.5,
/// ADR-8853): the state, the successors with their journal lag and budget, the full states and
/// the digest checks; and under `handoff` (ADR-8901) the endpoint's side of the hand-off: the
/// roster, the thresholds, the last hand-off's time and, on a promoted successor, who resumed.
pub fn migration(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let state = world.get_resource::<NetAgentState>();
    // A label may be published by the journal, by the hand-off, or by both.
    let labels: std::collections::BTreeSet<&str> = state.map_or_else(Default::default, |s| {
        s.migration()
            .keys()
            .chain(s.handoff().keys())
            .map(String::as_str)
            .collect()
    });
    if let Some(label) = target
        && !labels.contains(label)
    {
        let known: Vec<&str> = labels.iter().copied().collect();
        return Err(RpcError::new(
            NOT_FOUND,
            "migration_unknown_endpoint",
            format!("no host journal or successor named `{label}`"),
            format!("published: {known:?} (net.migration without `endpoint` lists them)"),
        ));
    }
    let endpoints: Vec<Value> = state
        .map(|s| {
            labels
                .iter()
                .filter(|label| target.is_none_or(|t| t == **label))
                .map(|label| {
                    // An endpoint the journal did not publish reports the journal's defaults
                    // (role and state empty) next to its hand-off.
                    let journal = s.migration().get(*label).cloned().unwrap_or_default();
                    let mut v = serde_json::to_value(journal).unwrap_or(Value::Null);
                    if let Value::Object(map) = &mut v {
                        map.insert("label".into(), json!(label));
                        map.insert(
                            "handoff".into(),
                            s.handoff()
                                .get(*label)
                                .and_then(|h| serde_json::to_value(h).ok())
                                .unwrap_or(Value::Null),
                        );
                    }
                    v
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "endpoints": endpoints }))
}

/// `net.conditioner_set`: queues a conditioner change (a named profile, then field overrides) for
/// one endpoint or for all; the endpoints apply it when they next publish.
pub fn conditioner_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let target = endpoint_param(params)?;
    let spec = match params.get("spec") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.as_str()),
        Some(_) => return Err(RpcError::invalid_params("`spec` must be a string")),
    };
    if spec.is_some() && params.get("profile").is_some_and(|v| !v.is_null()) {
        return Err(bad_conditioner("give `spec` or `profile`, not both"));
    }
    let mut config = match (spec, params.get("profile")) {
        // Stated conditions as one line, the same grammar as `--conditioner` and
        // `REGIA_NET_CONDITIONER` (ADR-3075).
        (Some(text), _) => ConditionerConfig::parse_spec(text).map_err(|e| bad_conditioner(&e))?,
        (None, None | Some(Value::Null)) => ConditionerConfig {
            enabled: true,
            ..ConditionerConfig::default()
        },
        (None, Some(Value::String(name))) => ConditionerConfig::from_profile(name).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "net_unknown_profile",
                format!("no conditioner profile `{name}`"),
                format!("profiles: {}", CONDITIONER_PROFILES.join(", ")),
            )
        })?,
        (None, Some(_)) => return Err(RpcError::invalid_params("`profile` must be a string")),
    };
    if let Some(overrides) = params.get("config").filter(|v| !v.is_null()) {
        let Value::Object(fields) = overrides else {
            return Err(bad_conditioner("`config` must be an object"));
        };
        let mut merged = serde_json::to_value(&config).unwrap_or(Value::Null);
        if let Value::Object(base) = &mut merged {
            for (key, value) in fields {
                if !base.contains_key(key) {
                    return Err(bad_conditioner(&format!("unknown conditioner field `{key}`")));
                }
                base.insert(key.clone(), value.clone());
            }
        }
        config = serde_json::from_value(merged).map_err(|e| bad_conditioner(&e.to_string()))?;
    }
    let config = config.sanitized();
    let Some(mut state) = world.get_resource_mut::<NetAgentState>() else {
        return Err(no_endpoint());
    };
    if state.reports().is_empty() {
        return Err(no_endpoint());
    }
    if let Some(label) = target
        && !state.reports().contains_key(label)
    {
        return Err(unknown_endpoint(label, Some(&state)));
    }
    let queued = state.request_conditioner(target, config.clone());
    Ok(json!({
        "queued_for": queued,
        "conditioner": serde_json::to_value(&config).unwrap_or(Value::Null),
        "conditions": config.describe(),
    }))
}

fn bad_conditioner(why: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "net_bad_conditioner",
        format!("invalid conditioner config: {why}"),
        "fields: enabled, seed, loss, duplicate, reorder, reorder_max_ms, delay_ms, jitter_ms, \
         burst_start_ms, burst_len_ms, burst_loss",
    )
}

/// `net.rollback`: game-owned rollback counters the host published (replays, deepest replay, too-old refusals).
///
/// An app with no [`NetAgentState`], or one that has not published, answers `published: false` and zeros.
pub fn rollback(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(state) = world.get_resource::<NetAgentState>() else {
        return Ok(json!({
            "published": false,
            "replays": 0,
            "depth": 0,
            "too_old": 0,
        }));
    };
    let counters = state.rollback();
    Ok(json!({
        "published": state.rollback_published(),
        "replays": counters.replays,
        "depth": counters.max_depth,
        "too_old": counters.too_old,
    }))
}

fn no_endpoint() -> RpcError {
    RpcError::new(
        UNSUPPORTED,
        "net_no_endpoint",
        "no network endpoint is running in this app",
        "the game must insert regia_net::NetAgentState and call NetServer::publish or NetClient::publish each frame",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::{EndpointReport, NetStatus};

    fn code(err: &RpcError) -> String {
        format!("{err:?}")
    }

    /// `net.resume` reads the server's resume state (spec 120 § 10.3): empty before any dormancy,
    /// then the dormant peer, the counters and whether a hook is set.
    #[test]
    fn resume_reports_dormant_peers_and_counters() {
        let mut world = World::new();
        let empty = resume(&mut world, &json!({})).expect("ok");
        assert_eq!(empty["dormant"], json!([]));
        assert_eq!(empty["hook"], false);
        assert_eq!(empty["last_resume"], Value::Null);
        let mut st = regia_net::NetResumeState::default();
        st.dormant.insert(
            regia_net::PeerId(3),
            regia_net::resume::DormantPeer { since_tick: 40 },
        );
        st.went_dormant = 2;
        st.resumed = 1;
        st.hook_calls = 1;
        st.last_resume = Some((regia_net::PeerId(2), 99));
        world.insert_resource(st);
        world.insert_resource(regia_net::NetGraceHook::new(|_, _| regia_net::GraceVerdict::Keep));
        let out = resume(&mut world, &json!({})).expect("ok");
        assert_eq!(out["dormant"], json!([{ "peer": 3, "since_tick": 40 }]));
        assert_eq!(
            (out["went_dormant"].clone(), out["resumed"].clone()),
            (json!(2), json!(1))
        );
        assert_eq!(out["hook"], true);
        assert_eq!(out["hook_calls"], 1);
        assert_eq!(out["last_resume"], json!({ "peer": 2, "tick": 99 }));
    }

    fn report(role: &str) -> EndpointReport {
        EndpointReport {
            status: NetStatus {
                interrupted: false,
                role: role.into(),
                state: "listening".into(),
                local_addr: Some("127.0.0.1:7777".into()),
                remote_addr: None,
                peer_id: None,
                tick: 12,
                tick_rate_hz: 30,
                peer_count: 0,
                max_peers: 8,
                conditioner: ConditionerConfig::default(),
                conditioner_stats: Default::default(),
                totals: Default::default(),
                link: None,
                crypto: Default::default(),
                version_mismatch: None,
                input_timing: None,
                reconnect: None,
                streams: Vec::new(),
                socket: None,
                resume_refusals: Default::default(),
                quality: None,
            },
            peers: Vec::new(),
            rejections: Vec::new(),
        }
    }

    /// KI-NT-025 (ADR-10077): `net.rejections` lists each peer's refusals by lowercase code, per
    /// server endpoint (the transport's) and on a server world (the runtime's), filtered by
    /// `endpoint` and `peer`; a bad `peer` and an unknown endpoint are named errors.
    #[test]
    fn rejections_lists_refusals_per_peer_by_code() {
        use regia_net::model::{NetMode, PeerId};
        use regia_net::server::RejectKind;

        let mut world = World::new();
        let mut state = NetAgentState::default();
        let mut server = report("server");
        let mut by = regia_net::RejectionsByPeer::default();
        by.note(4, "flood");
        by.note(7, "message_malformed");
        by.note(7, "message_malformed");
        server.rejections = by.rows();
        state.publish("server", server);
        state.publish("client", report("client"));
        world.insert_resource(state);

        let all = rejections(&mut world, &json!({})).expect("net.rejections");
        crate::contract::assert_result_named("net.rejections", &all);
        assert_eq!(all["runtime"], Value::Null, "no net model in this world");
        let endpoints = all["endpoints"].as_array().expect("endpoints");
        assert_eq!(endpoints.len(), 2);
        let server = endpoints
            .iter()
            .find(|e| e["label"] == "server")
            .expect("server endpoint");
        assert_eq!(
            server["peers"],
            json!([
                { "peer": 4, "total": 1, "kinds": { "flood": 1 } },
                { "peer": 7, "total": 2, "kinds": { "message_malformed": 2 } },
            ])
        );
        let client = endpoints
            .iter()
            .find(|e| e["label"] == "client")
            .expect("client endpoint");
        assert_eq!(client["peers"], json!([]));

        let one = rejections(&mut world, &json!({ "endpoint": "server", "peer": 7 })).expect("filtered");
        assert_eq!(one["endpoints"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            one["endpoints"][0]["peers"],
            json!([{ "peer": 7, "total": 2, "kinds": { "message_malformed": 2 } }])
        );

        let err = rejections(&mut world, &json!({ "endpoint": "nope" })).expect_err("unknown");
        assert!(code(&err).contains("net_unknown_endpoint"), "{err:?}");
        for bad in [json!("7"), json!(-1), json!(4_294_967_296u64), json!(1.5)] {
            let err = rejections(&mut world, &json!({ "peer": bad })).expect_err("bad peer");
            assert!(code(&err).contains("net_rejections_bad_peer"), "{bad}: {err:?}");
        }

        // The replication runtime's refusals on a server world.
        world.insert_resource(NetMode::DedicatedServer);
        regia_net::runtime::ensure_resources(&mut world);
        regia_net::runtime::server_add_peer(&mut world, PeerId(2));
        assert!(world.resource_mut::<regia_net::runtime::NetNode>().reject(
            PeerId(2),
            RejectKind::ServerOnlyMessage,
            "Forged"
        ));
        let taken = world
            .resource_mut::<regia_net::runtime::NetNode>()
            .take_rejections();
        assert_eq!(taken.len(), 1, "the log is taken; the counts stay");
        let out = rejections(&mut world, &json!({})).expect("net.rejections");
        crate::contract::assert_result_named("net.rejections", &out);
        assert_eq!(
            out["runtime"],
            json!({
                "total": 1,
                "dropped": 0,
                "peers": [{ "peer": 2, "total": 1, "kinds": { "server_only_message": 1 } }],
            })
        );
        let none = rejections(&mut world, &json!({ "peer": 9 })).expect("no such peer");
        assert_eq!(none["runtime"]["peers"], json!([]));
    }

    /// Wire 6 (ADR-8902): `net.status` shows the one-round-trip resume path: how many resumes a
    /// server admitted at their key confirmation, and why it refused the others, by lowercase
    /// code. The map is absent while empty.
    #[test]
    fn status_shows_one_round_trip_resumes_and_refusals_by_code() {
        let mut world = World::new();
        let mut state = NetAgentState::default();
        let mut server = report("server");
        state.publish("quiet", server.clone());
        server.status.totals.resume_inits = 5;
        server.status.totals.resumed_one_rtt = 2;
        server.status.totals.resume_replays = 1;
        server.status.totals.resume_lapsed = 1;
        server.status.resume_refusals.insert("resume_epoch".into(), 2);
        server
            .status
            .resume_refusals
            .insert("resume_stale_flight".into(), 1);
        state.publish("server", server);
        world.insert_resource(state);
        let out = status(&mut world, &json!({})).expect("net.status");
        let endpoints = out["endpoints"].as_array().expect("endpoints");
        let find = |label: &str| {
            endpoints
                .iter()
                .find(|e| e["label"] == label)
                .unwrap_or_else(|| panic!("endpoint {label}: {out}"))
        };
        let s = find("server");
        assert_eq!(s["totals"]["resumed_one_rtt"], json!(2), "{out}");
        assert_eq!(s["totals"]["resume_replays"], json!(1));
        assert_eq!(s["totals"]["resume_lapsed"], json!(1));
        assert_eq!(
            s["resume_refusals"],
            json!({ "resume_epoch": 2, "resume_stale_flight": 1 })
        );
        for code in s["resume_refusals"].as_object().expect("map").keys() {
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
        }
        let quiet = find("quiet");
        assert_eq!(quiet["totals"]["resumed_one_rtt"], json!(0));
        assert!(
            quiet.get("resume_refusals").is_none(),
            "absent while empty: {quiet}"
        );
    }

    #[test]
    fn interruption_reaches_status_peers_and_overlay() {
        let mut world = World::new();
        let mut state = NetAgentState::default();
        let mut server = report("server");
        server.status.interrupted = true;
        server.peers.push(regia_net::PeerRow {
            interrupted: true,
            id: 7,
            name: "loader".into(),
            role: "player".into(),
            addr: "127.0.0.1:7000".into(),
            rtt_ms: 30.0,
            jitter_ms: 1.0,
            loss_fraction: 0.0,
            silent_ms: 1_500,
            flagged: false,
            missed_input_ticks: 0,
            inputs_too_far_ahead: 0,
            input_feedback: None,
            grade: regia_net::LinkGrade::Interrupted,
            quality: None,
            link: Default::default(),
            sealed: true,
            rebinds: 0,
            rebinds_refused: 0,
            path_candidate: None,
            path_probes: 0,
            path_mirrored: 0,
            inbound: Default::default(),
            streams: vec![regia_net::connection::StreamStatus {
                stream: regia_net::connection::StreamId::BULK,
                spec: regia_net::connection::ENGINE_STREAMS[6],
                queued_fragments: 3,
                send_credit_bytes: 4_096,
                buffered_bytes: 1_186,
                assembling_bytes: 0,
                stall_ms: 40,
                class: regia_net::connection::StreamClass::Bulk,
                msgs_capped: 2,
                msgs_last_second: 0,
            }],
        });
        state.publish("server", server);
        world.insert_resource(state);
        assert_eq!(
            status(&mut world, &json!({})).expect("status")["endpoints"][0]["interrupted"],
            true
        );
        // ADR-8302: each peer row carries its link's reliable streams.
        let rows = peers(&mut world, &json!({"endpoint": "server"})).expect("peers");
        let bulk = &rows["endpoints"][0]["peers"][0]["streams"][0];
        assert_eq!(bulk["stream"], json!(6), "{rows}");
        assert_eq!(bulk["spec"]["window_bytes"], json!(1024 * 1024));
        assert_eq!(bulk["buffered_bytes"], json!(1_186));
        assert_eq!(bulk["stall_ms"], json!(40));
        // ADR-8407: the row names its class and carries the class's message cap counters.
        assert_eq!(bulk["class"], json!("bulk"));
        assert_eq!(bulk["msgs_capped"], json!(2));
        assert_eq!(bulk["spec"]["msgs_per_s"], json!(0));
        assert_eq!(
            peers(&mut world, &json!({"endpoint": "server"})).expect("peers")["endpoints"][0]["peers"][0]["interrupted"],
            true
        );
        let report = &world.resource::<NetAgentState>().reports()["server"];
        let overlay = regia_net::DiagnosticsOverlay::new(report.status.clone(), report.peers.clone());
        assert!(overlay.lines().iter().any(|line| line.contains("INTERRUPTED")));
    }

    #[test]
    fn crypto_reports_each_endpoint_and_refuses_an_unknown_one() {
        let mut world = World::new();
        assert_eq!(
            crypto(&mut world, &json!({})).expect("ok")["endpoints"],
            json!([])
        );
        let mut state = NetAgentState::default();
        let mut server = report("server");
        server.status.crypto.mode = "auto".into();
        server.status.crypto.sealed_peers = 2;
        server.status.totals.replayed = 3;
        server.status.totals.auth_failed = 4;
        state.publish("server", server);
        state.publish("client", report("client"));
        world.insert_resource(state);
        let all = crypto(&mut world, &json!({})).expect("ok");
        assert_eq!(all["endpoints"].as_array().map(Vec::len), Some(2));
        let one = crypto(&mut world, &json!({ "endpoint": "server" })).expect("ok");
        let row = &one["endpoints"][0];
        assert_eq!(row["label"], json!("server"));
        assert_eq!(row["mode"], json!("auto"));
        assert_eq!(row["sealed_peers"], json!(2));
        assert_eq!(row["counters"]["replayed"], json!(3));
        assert_eq!(row["counters"]["auth_failed"], json!(4));
        let err = crypto(&mut world, &json!({ "endpoint": "nope" })).expect_err("unknown");
        assert!(code(&err).contains("net_unknown_endpoint"), "{err:?}");
    }

    #[test]
    fn without_the_bridge_reads_are_empty_and_set_is_refused() {
        let mut world = World::new();
        let s = status(&mut world, &json!({})).expect("ok");
        assert_eq!(s["bridge"], json!(false));
        let p = peers(&mut world, &json!({})).expect("ok");
        assert_eq!(p["endpoints"], json!([]));
        let err = conditioner_set(&mut world, &json!({ "profile": "wan" })).expect_err("no endpoint");
        assert!(code(&err).contains("net_no_endpoint"), "{err:?}");
    }

    struct Game;
    impl regia_net::NetGame for Game {
        type Message = Vec<u8>;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "regia.agent.net_test";
        const PROTOCOL_VERSION: u32 = 1;
    }

    #[test]
    fn future_input_rejections_reach_status_and_peer_handlers() {
        use regia_net::{ClientConfig, LoopbackNet, NetClient, NetServer, ServerConfig};
        use std::net::SocketAddr;
        let net = LoopbackNet::new();
        let addr = SocketAddr::from(([127, 0, 0, 1], 7777));
        let mut server: NetServer<Game, _> = NetServer::with_secret(
            net.bind(addr).expect("bind"),
            ServerConfig {
                input_buffer_ticks: 4,
                ..ServerConfig::default()
            },
            [7; 32],
        );
        let mut client: NetClient<Game, _> = NetClient::connect(
            net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
            addr,
            ClientConfig {
                input_redundancy: 1,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("join");
        for now in 0..20 {
            client.update(now);
            server.update(now);
        }
        assert!(client.is_connected());
        client.send_input(4, 7).expect("queued");
        client.update(21);
        server.update(21);
        let mut state = NetAgentState::default();
        server.publish("server", &mut state);
        let mut world = World::new();
        world.insert_resource(state);
        let s = status(&mut world, &json!({})).expect("net.status");
        assert_eq!(s["endpoints"][0]["totals"]["inputs_too_far_ahead"], json!(1));
        let p = peers(&mut world, &json!({"endpoint": "server"})).expect("net.peers");
        assert_eq!(p["endpoints"][0]["peers"][0]["inputs_too_far_ahead"], json!(1));
    }

    /// ADR-8361: a rollback lockstep seat reports its rollbacks, the deepest one, the ticks it
    /// predicted and the lag between sampling and stepping its local input on `net.lockstep`.
    #[test]
    fn net_lockstep_reports_a_rollback_seats_counters() {
        use regia_net::lockstep::net::{publish_host, publish_seat};
        use regia_net::lockstep::{
            HostRole, LockstepConfig, LockstepFrame, LockstepHost, LockstepSeat, RollbackGame,
            RollbackRunner, SeatId,
        };

        struct Game;
        impl regia_net::NetGame for Game {
            type Message = Vec<u8>;
            type Input = u32;
            type JoinData = ();
            const GAME_ID: &'static str = "agent.rollback";
            const PROTOCOL_VERSION: u32 = 1;
        }
        #[derive(Clone, Default)]
        struct Sum(u64);
        impl RollbackGame<u32> for Sum {
            fn step(&mut self, frame: &LockstepFrame<u32>) {
                for (_, input) in &frame.inputs {
                    self.0 = self
                        .0
                        .wrapping_mul(31)
                        .wrapping_add(u64::from(*input.as_ref().unwrap_or(&0)));
                }
            }
            fn hash(&self) -> u64 {
                self.0
            }
        }

        let config = LockstepConfig::rollback(60);
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        let mut seats: Vec<_> = (0..2u16)
            .map(|s| {
                host.add_seat(SeatId(s));
                (
                    LockstepSeat::<Game>::new(config.clone(), SeatId(s)),
                    RollbackRunner::<Game, Sum>::new(&config, SeatId(s), Sum::default()),
                )
            })
            .collect();
        host.start(0);
        // Two seconds of play, each seat 40 ms from the host; every input differs from the last.
        let mut to_host: Vec<(u64, u16, regia_net::lockstep::Outgoing)> = Vec::new();
        let mut to_seat: Vec<(u64, u16, regia_net::lockstep::Outgoing)> = Vec::new();
        for now in 0..2_000u64 {
            for (_, s, out) in to_host.extract_if(.., |(at, _, _)| *at <= now) {
                host.receive(SeatId(s), out.lane, &out.bytes, now)
                    .expect("seat payload");
            }
            while host.poll_frame(now).is_some() {}
            to_seat.extend(host.outgoing(now).into_iter().map(|(to, o)| (now + 40, to.0, o)));
            if now.is_multiple_of(16) {
                for (s, (seat, runner)) in seats.iter_mut().enumerate() {
                    for (_, _, out) in
                        to_seat.extract_if(.., |(at, to, _)| *at <= now && usize::from(*to) == s)
                    {
                        seat.receive(out.lane, &out.bytes, now).expect("host payload");
                    }
                    runner.advance(seat, now, |capture| capture as u32 * 7 + s as u32, |_, _| {});
                    to_host.extend(seat.outgoing(now).into_iter().map(|o| (now + 40, s as u16, o)));
                }
            }
        }
        let mut state = NetAgentState::default();
        publish_host(&mut host, "host", 2_000, &mut state);
        publish_seat(&seats[0].0, "seat0", &mut state);
        let mut world = World::new();
        world.insert_resource(state);

        let h = lockstep(&mut world, &json!({"endpoint": "host"})).expect("net.lockstep host");
        assert_eq!(h["endpoints"][0]["rollback_window_ticks"], json!(8));
        let s = lockstep(&mut world, &json!({"endpoint": "seat0"})).expect("net.lockstep seat");
        let s = &s["endpoints"][0];
        assert_eq!(s["rollback_window_ticks"], json!(8));
        assert!(s["rollbacks"].as_u64().is_some_and(|r| r > 10), "{s}");
        assert!(
            s["rollback_ticks_max"]
                .as_u64()
                .is_some_and(|r| (1..=8).contains(&r)),
            "{s}"
        );
        assert!(s["ticks_predicted"].as_u64().is_some_and(|t| t > 50), "{s}");
        assert!(
            s.get("window_stalls").is_some() && s.get("ahead_ticks").is_some(),
            "{s}"
        );
        let lag = s["local_input_lag_ticks"].as_array().cloned().unwrap_or_default();
        assert_eq!(lag.len(), 2, "{s}");
        assert!(
            lag.iter()
                .all(|t| t.as_u64().is_some_and(|t| (1..=3).contains(&t))),
            "{s}"
        );
    }

    /// ADR-8853: a host journal and its successor publish through `JournalHost::publish` /
    /// `JournalSuccessor::publish`, and `net.migration` reports the state, the successors and the
    /// journal lag; an unknown label is `migration_unknown_endpoint`.
    #[test]
    fn net_migration_reports_a_host_journal_and_its_successor() {
        use regia_net::PeerId;
        use regia_net::migration::{
            Candidate, JournalConfig, JournalHost, JournalSuccessor, ShadowWorld, WorldDigest,
        };

        /// A world of one counter: the sum of every input byte.
        #[derive(Default)]
        struct Sum(u64);
        impl Sum {
            fn digest_of(v: u64) -> WorldDigest {
                let mut d = [0; 32];
                d[..8].copy_from_slice(&v.to_le_bytes());
                d
            }
        }
        impl ShadowWorld for Sum {
            fn load_full(&mut self, _tick: u64, bytes: &[u8]) -> Result<(), String> {
                let bytes: [u8; 8] = bytes.try_into().map_err(|_| "8 bytes".to_owned())?;
                self.0 = u64::from_le_bytes(bytes);
                Ok(())
            }
            fn apply(&mut self, _tick: u64, inputs: &[u8], _events: &[u8]) -> Result<(), String> {
                self.0 += inputs.iter().map(|b| u64::from(*b)).sum::<u64>();
                Ok(())
            }
            fn digest(&self) -> WorldDigest {
                Self::digest_of(self.0)
            }
        }

        let mut host = JournalHost::new(JournalConfig::default());
        let mut shadow = JournalSuccessor::new(Sum::default());
        let candidates = [Candidate {
            peer: PeerId(7),
            rtt_ms: 20.0,
            jitter_ms: 1.0,
            loss_fraction: 0.0,
            interrupted: false,
            allowance_bytes_per_s: Some(64 * 1024),
        }];
        let mut sum = 0u64;
        // Twelve seconds at 60 Hz: the seed and two more full states.
        for tick in 1..=720u64 {
            let now = tick * 1_000 / 60;
            sum += tick % 5;
            host.set_candidates(now, &candidates);
            if host.full_wanted(now) {
                let digest = Sum::digest_of(sum);
                host.record_tick(tick, &[(tick % 5) as u8], &[], Some(digest))
                    .expect("record_tick");
                host.record_full(now, tick, sum.to_le_bytes().to_vec(), digest)
                    .expect("record_full");
            } else {
                host.record_tick(tick, &[(tick % 5) as u8], &[], None)
                    .expect("record_tick");
            }
            for out in host.poll(now, |_| Some(64 * 1024)) {
                shadow.on_payload(&out.bytes).expect("journal payload");
            }
            if let Some(ack) = shadow.poll_ack() {
                host.on_payload(PeerId(7), &ack).expect("ack");
            }
        }
        let mut state = NetAgentState::default();
        host.publish("server", &mut state);
        shadow.publish("client7", &mut state);
        let mut world = World::new();
        world.insert_resource(state);

        let all = migration(&mut world, &json!({})).expect("net.migration");
        assert_eq!(all["endpoints"].as_array().map(Vec::len), Some(2));
        let h = migration(&mut world, &json!({"endpoint": "server"})).expect("net.migration host");
        let h = &h["endpoints"][0];
        assert_eq!(h["label"], "server");
        assert_eq!(h["role"], "host");
        assert_eq!(h["state"], "warm");
        assert_eq!(h["tick"], 720);
        assert_eq!(h["full_states"], 3);
        assert_eq!(h["successors"][0]["peer"], 7);
        assert_eq!(h["successors"][0]["rank"], 0);
        assert_eq!(h["successors"][0]["state"], "warm");
        assert_eq!(h["successors"][0]["acked_tick"], 720);
        assert_eq!(h["journal_lag_ticks"], 0);
        assert_eq!(h["successors"][0]["seconds_over_budget"], 0);
        let s = migration(&mut world, &json!({"endpoint": "client7"})).expect("net.migration successor");
        let s = &s["endpoints"][0];
        assert_eq!(s["role"], "successor");
        assert_eq!(s["state"], "warm");
        assert_eq!(s["rank"], 0);
        assert_eq!(s["tick"], 720);
        assert_eq!(s["promotable"], true);
        assert_eq!(s["replay_divergences"], 0);
        assert_eq!(s["digest_checks"], 3);
        let err = migration(&mut world, &json!({"endpoint": "nobody"})).expect_err("unknown label");
        assert!(code(&err).contains("migration_unknown_endpoint"), "{err:?}");
        assert!(migration(&mut world, &json!({"endpoint": 5})).is_err());
        // Without the resource the list is empty.
        let mut empty = World::new();
        let none = migration(&mut empty, &json!({})).expect("net.migration");
        assert_eq!(none["endpoints"].as_array().map(Vec::len), Some(0));
    }

    /// ADR-8901: a host's and a client's hand-off publish through `HandoffHost::publish` /
    /// `HandoffClient::publish`, and `net.migration` reports them under `handoff`: the roster
    /// with each successor's address and key, the thresholds, the client's rank and whether it
    /// holds its grant; a label only the hand-off published is listed too.
    #[test]
    fn net_migration_reports_the_hand_off() {
        use regia_net::handoff::{HandoffClient, HandoffConfig, HandoffHost};
        use regia_net::{PeerId, ServerIdentity};

        let identity = ServerIdentity::derived(&[3; 32]).expect("key pair");
        let key = identity.public().to_string();
        let mut host = HandoffHost::with_secret(HandoffConfig::default(), [7; 32]);
        let addr = |last: u8| std::net::SocketAddr::from(([10, 0, 0, last], 5000));
        host.set_peers(&[(PeerId(1), addr(11)), (PeerId(2), addr(12))]);
        host.set_successors(&[PeerId(1)]);
        let mut successor = HandoffClient::new(HandoffConfig::default()).with_identity(identity);
        successor.set_peer(PeerId(1));
        assert!(successor.set_appointed(true));
        let announce = successor.poll_out().expect("announcement");
        host.on_payload(PeerId(1), &announce).expect("announce");
        let mut other = HandoffClient::new(HandoffConfig::default());
        other.set_peer(PeerId(2));
        for out in host.poll() {
            let client = if out.peer == PeerId(1) {
                &mut successor
            } else {
                &mut other
            };
            client.on_payload(&out.bytes).expect("roster or grant");
        }
        let mut state = NetAgentState::default();
        host.publish("server", &mut state);
        successor.publish("client1", &mut state);
        other.publish("client2", &mut state);
        let mut world = World::new();
        world.insert_resource(state);

        let all = migration(&mut world, &json!({})).expect("net.migration");
        assert_eq!(all["endpoints"].as_array().map(Vec::len), Some(3));
        let h = migration(&mut world, &json!({"endpoint": "server"})).expect("host");
        let h = &h["endpoints"][0]["handoff"];
        assert_eq!(h["role"], "host");
        assert_eq!(h["state"], "ready");
        assert_eq!(h["epoch"], 1);
        assert_eq!(h["host_lost_ms"], 1500);
        assert_eq!(h["takeover_ms"], 3000);
        assert_eq!(h["successors"][0]["peer"], 1);
        assert_eq!(h["successors"][0]["rank"], 0);
        assert_eq!(h["successors"][0]["addr"], "10.0.0.11:5000");
        assert_eq!(h["successors"][0]["key"], key.as_str());
        assert_eq!(h["rosters_sent"], 2);
        assert_eq!(h["grants_sent"], 1);
        let s = migration(&mut world, &json!({"endpoint": "client1"})).expect("successor");
        let s = &s["endpoints"][0]["handoff"];
        assert_eq!(s["role"], "successor");
        assert_eq!(s["state"], "watching");
        assert_eq!(s["rank"], 0);
        assert_eq!(s["granted"], true);
        assert_eq!(s["host_losses"], 0);
        assert_eq!(s["last_handoff_ms"], Value::Null);
        let c = migration(&mut world, &json!({"endpoint": "client2"})).expect("client");
        let c = &c["endpoints"][0];
        assert_eq!(c["label"], "client2");
        assert_eq!(c["handoff"]["role"], "client");
        assert_eq!(c["handoff"]["rank"], Value::Null);
        assert_eq!(c["handoff"]["successors"][0]["key"], key.as_str());
        // The serialized report never carries a ticket or the grant's secret.
        let text = all.to_string();
        assert!(!text.contains("ticket") && !text.contains("secret"), "{text}");
        let err = migration(&mut world, &json!({"endpoint": "nobody"})).expect_err("unknown label");
        assert!(code(&err).contains("migration_unknown_endpoint"), "{err:?}");
    }

    /// ADR-8146: a live lockstep host and seat publish through `publish_host` / `publish_seat`, and
    /// `net.lockstep` reports the delay, the frame lag per seat and the resync counters; an unknown
    /// label is `lockstep_unknown_endpoint`.
    #[test]
    fn net_lockstep_reports_a_live_host_and_seat() {
        use regia_net::lockstep::net::{publish_host, publish_seat};
        use regia_net::lockstep::{HostRole, LockstepConfig, LockstepHost, LockstepSeat, SeatId};

        struct Game;
        impl regia_net::NetGame for Game {
            type Message = Vec<u8>;
            type Input = u32;
            type JoinData = ();
            const GAME_ID: &'static str = "agent.lockstep";
            const PROTOCOL_VERSION: u32 = 1;
        }

        let config = LockstepConfig::default();
        let mut host = LockstepHost::<Game>::new(config.clone(), HostRole::Relay);
        let mut seat = LockstepSeat::<Game>::new(config, SeatId(3));
        host.add_seat(SeatId(3));
        host.start(0);
        // Two seconds of play, the seat 40 ms behind the host.
        let mut to_host: Vec<(u64, regia_net::lockstep::Outgoing)> = Vec::new();
        let mut to_seat: Vec<(u64, regia_net::lockstep::Outgoing)> = Vec::new();
        let mut stepped = 0u64;
        seat.submit_input(&1).expect("capture 0");
        for now in 0..2_000u64 {
            for (_, out) in to_host.extract_if(.., |(at, _)| *at <= now) {
                host.receive(SeatId(3), out.lane, &out.bytes, now)
                    .expect("seat payload");
            }
            for (_, out) in to_seat.extract_if(.., |(at, _)| *at <= now) {
                seat.receive(out.lane, &out.bytes, now).expect("host payload");
            }
            while let Some(frame) = seat.next_frame() {
                stepped += 1;
                seat.submit_input(&(frame.tick as u32))
                    .expect("one input per capture");
            }
            if now.is_multiple_of(16) {
                to_host.extend(seat.outgoing(now).into_iter().map(|o| (now + 40, o)));
                while host.poll_frame(now).is_some() {}
                to_seat.extend(host.outgoing(now).into_iter().map(|(_, o)| (now + 40, o)));
            }
        }
        assert!(stepped > 100);
        let mut state = NetAgentState::default();
        publish_host(&mut host, "host", 2_000, &mut state);
        publish_seat(&seat, "seat3", &mut state);
        let mut world = World::new();
        world.insert_resource(state);

        let all = lockstep(&mut world, &json!({})).expect("net.lockstep");
        assert_eq!(all["endpoints"].as_array().map(Vec::len), Some(2));
        let h = lockstep(&mut world, &json!({"endpoint": "host"})).expect("net.lockstep host");
        let h = &h["endpoints"][0];
        assert_eq!(h["label"], json!("host"));
        assert_eq!(h["role"], json!("host"));
        assert!(
            h["delay_ticks"].as_u64().is_some_and(|d| d >= 5),
            "80 ms round trips: {h}"
        );
        assert_eq!(h["seats"][0]["seat"], json!(3));
        assert!(
            h["seats"][0]["frame_lag_ticks"]
                .as_u64()
                .is_some_and(|l| (3..=9).contains(&l)),
            "about a round trip of frames: {h}"
        );
        assert_eq!(h["resyncs"], json!(0));
        assert_eq!(h["desyncs"], json!(0));
        // The desync probe's report fields (spec 120 § 7.6, ADR-8211): none on a clean match.
        assert_eq!(h["desync_reports"], json!(0));
        assert_eq!(h["last_desync"], json!(null));
        // ADR-8209: the late-stamp flag is on the agent surface (an honest seat is not listed).
        assert_eq!(h["late_stamp_suspects"], json!([]));
        assert_eq!(h["seats"][0]["late_stamp_suspect"], json!(false));
        assert_eq!(h["seats"][0]["late_stamp_flags"], json!(0));
        assert!(h["seats"][0].get("input_hold_p10_us").is_some(), "{h}");
        assert!(h["seats"][0].get("input_margin_p50_us").is_some(), "{h}");
        let s = lockstep(&mut world, &json!({"endpoint": "seat3"})).expect("net.lockstep seat");
        assert_eq!(s["endpoints"][0]["role"], json!("seat"));
        assert_eq!(s["endpoints"][0]["delay_ticks"], h["delay_ticks"]);

        let err = lockstep(&mut world, &json!({"endpoint": "nope"})).expect_err("unknown label");
        assert!(code(&err).contains("lockstep_unknown_endpoint"), "{err:?}");
        // Without the bridge resource the answer is an empty list, like net.status.
        let mut empty = World::new();
        assert_eq!(
            lockstep(&mut empty, &json!({})).expect("empty"),
            json!({ "endpoints": [] })
        );
    }

    /// ADR-8212: `net.lockstep` shows the session half (spec 120 § 7.4): a silent seat is a member
    /// that is interrupted with its silence in milliseconds, a seat that never comes is removed at
    /// the timeout and counted, and the tick the match ended on.
    #[test]
    fn net_lockstep_reports_silent_seats_removals_and_the_match_end() {
        use regia_net::lockstep::net::publish_host;
        use regia_net::lockstep::{HostRole, LockstepConfig, LockstepHost, SeatId, TimeoutPolicy};

        struct Game;
        impl regia_net::NetGame for Game {
            type Message = Vec<u8>;
            type Input = u32;
            type JoinData = ();
            const GAME_ID: &'static str = "agent.lockstep.sessions";
            const PROTOCOL_VERSION: u32 = 1;
        }

        // Admitted seats, not loaders. Silence counts from admission (spec 120 § 7.4): a
        // start grace would keep an unheard seat from being interrupted at 3 s. The loader
        // case is the session test that keeps a seat inside the grace and removes it at the grace.
        let config = LockstepConfig {
            timeout: TimeoutPolicy {
                timeout_ms: 5_000,
                start_grace_ms: 0,
                ..TimeoutPolicy::default()
            },
            ..LockstepConfig::default()
        };
        let mut host = LockstepHost::<Game>::new(config, HostRole::Relay);
        host.add_seat(SeatId(1));
        host.add_seat(SeatId(2));
        host.start(0);
        // Seat 1 is silent for 3 s: interrupted, still a member.
        for now in 0..3_000u64 {
            while host.poll_frame(now).is_some() {}
        }
        let mut state = NetAgentState::default();
        publish_host(&mut host, "host", 3_000, &mut state);
        let mut world = World::new();
        world.insert_resource(state);
        let h = lockstep(&mut world, &json!({"endpoint": "host"})).expect("net.lockstep");
        let row = &h["endpoints"][0]["seats"][0];
        assert_eq!(row["seat"], json!(1));
        assert_eq!(row["member"], json!(true));
        assert_eq!(row["interrupted"], json!(true), "{row}");
        assert!(row["silent_ms"].as_u64().is_some_and(|ms| ms >= 2_900), "{row}");
        assert_eq!(h["endpoints"][0]["match_end_tick"], json!(null));
        // Past the timeout both seats are removed; the match ends.
        for now in 3_000..6_000u64 {
            while host.poll_frame(now).is_some() {}
        }
        host.end_match();
        while host.poll_frame(6_000).is_some() {}
        let mut state = NetAgentState::default();
        publish_host(&mut host, "host", 6_000, &mut state);
        world.insert_resource(state);
        let h = lockstep(&mut world, &json!({"endpoint": "host"})).expect("net.lockstep");
        let e = &h["endpoints"][0];
        assert_eq!(e["seats_timed_out"], json!(2), "{e}");
        assert_eq!(e["match_end_tick"], json!(360), "{e}");
        assert_eq!(e["seats_left"], json!(0));
    }

    #[test]
    fn a_live_server_and_client_answer_through_the_handlers() {
        use regia_net::{ClientConfig, LoopbackNet, NetClient, NetServer, ServerConfig};
        use std::net::SocketAddr;

        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
        let mut server: NetServer<Game, _> =
            NetServer::new(net.bind(server_addr).expect("bind"), ServerConfig::default());
        let config = ClientConfig {
            name: "ada".into(),
            ..ClientConfig::default()
        };
        let mut client: NetClient<Game, _> = NetClient::connect(
            net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
            server_addr,
            config,
            &(),
        )
        .expect("join fits");
        let mut world = World::new();
        world.init_resource::<NetAgentState>();
        let mut now = 0;
        let mut frames = |world: &mut World, n: u32| {
            for _ in 0..n {
                now += 10;
                client.update(now);
                server.update(now);
                let mut state = world.resource_mut::<NetAgentState>();
                server.publish("server", &mut state);
                client.publish("client", &mut state);
            }
        };
        frames(&mut world, 30);
        let s = status(&mut world, &json!({})).expect("ok");
        let endpoints = s["endpoints"].as_array().expect("array");
        assert_eq!(endpoints.len(), 2);
        let by_label = |label: &str| endpoints.iter().find(|e| e["label"] == json!(label)).cloned();
        assert_eq!(by_label("server").expect("server")["peer_count"], json!(1));
        assert_eq!(by_label("client").expect("client")["state"], json!("connected"));
        let p = peers(&mut world, &json!({ "endpoint": "server" })).expect("ok");
        assert_eq!(p["endpoints"][0]["peers"][0]["name"], json!("ada"));
        // ADR-8182: net.peers rows carry the grade, net.quality the signals on both ends.
        assert_eq!(p["endpoints"][0]["peers"][0]["grade"], json!("good"));
        let q = quality(&mut world, &json!({})).expect("net.quality");
        let by = |label: &str| {
            q["endpoints"]
                .as_array()
                .and_then(|a| a.iter().find(|e| e["label"] == json!(label)).cloned())
                .expect(label)
        };
        assert_eq!(by("client")["grade"], json!("good"));
        assert_eq!(by("client")["quality"]["interrupted"], json!(false));
        assert!(by("client")["quality"]["rtt_p95_ms"].is_number());
        assert_eq!(by("server")["peers"][0]["name"], json!("ada"));
        assert_eq!(by("server")["peers"][0]["grade"], json!("good"));
        assert!(by("server")["peers"][0]["quality"]["loss_recv"].is_number());
        assert_eq!(q["limits"]["good"]["rtt_p95_ms"], json!(150.0));
        let id = by("server")["peers"][0]["id"].clone();
        let one = quality(&mut world, &json!({ "endpoint": "server", "peer": id })).expect("one peer");
        assert_eq!(one["endpoints"][0]["peers"].as_array().map(Vec::len), Some(1));
        let err = quality(&mut world, &json!({ "peer": 999 })).expect_err("unknown peer");
        assert!(code(&err).contains("peer_unknown"), "{err:?}");
        let err = quality(&mut world, &json!({ "endpoint": "nope" })).expect_err("unknown endpoint");
        assert!(code(&err).contains("net_unknown_endpoint"), "{err:?}");
        assert!(
            quality(&mut world, &json!({ "peer": "x" })).is_err(),
            "a peer id is an integer"
        );

        let set =
            conditioner_set(&mut world, &json!({ "endpoint": "client", "profile": "bad" })).expect("queued");
        assert_eq!(set["queued_for"], json!(["client"]));
        frames(&mut world, 30);
        let s = status(&mut world, &json!({})).expect("ok");
        let endpoints = s["endpoints"].as_array().expect("array");
        let client_status = endpoints
            .iter()
            .find(|e| e["label"] == json!("client"))
            .expect("client");
        let server_status = endpoints
            .iter()
            .find(|e| e["label"] == json!("server"))
            .expect("server");
        assert_eq!(
            client_status["conditioner"]["delay_ms"],
            json!(120),
            "the client took the profile"
        );
        assert!(
            client_status["conditioner_stats"]["delayed"]
                .as_u64()
                .is_some_and(|d| d > 0)
        );
        assert_eq!(
            server_status["conditioner"]["enabled"],
            json!(false),
            "the server was not targeted"
        );
    }

    /// ADR-8077: a client's input timing reaches `net.status` and the server's input feedback per
    /// player reaches `net.peers`.
    #[test]
    fn input_timing_reaches_status_and_peers() {
        use regia_net::{ClientConfig, LoopbackNet, NetClient, NetServer, ServerConfig, TickClock};
        use std::net::SocketAddr;

        let net = LoopbackNet::new();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], 7777));
        let mut server: NetServer<Game, _> =
            NetServer::new(net.bind(server_addr).expect("bind"), ServerConfig::default());
        let mut client: NetClient<Game, _> = NetClient::connect(
            net.bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind"),
            server_addr,
            ClientConfig::default(),
            &(),
        )
        .expect("join fits");
        let mut world = World::new();
        world.init_resource::<NetAgentState>();
        let mut ticker = TickClock::new(server.config().tick_rate_hz, 4);
        for step in 1..=300u64 {
            let now = step * 10;
            client.receive(now);
            for tick in client.due_input_ticks(now) {
                client.send_input(tick, 1).expect("queued");
            }
            client.flush(now);
            server.receive(now);
            for _ in 0..ticker.advance(10) {
                server.advance_tick(|_, _| {});
            }
            server.flush(now);
            let mut state = world.resource_mut::<NetAgentState>();
            server.publish("server", &mut state);
            client.publish("client", &mut state);
        }
        let s = status(&mut world, &json!({})).expect("ok");
        let client_status = s["endpoints"]
            .as_array()
            .expect("array")
            .iter()
            .find(|e| e["label"] == json!("client"))
            .cloned()
            .expect("client");
        let timing = &client_status["input_timing"];
        assert!(timing["lead_ticks"].as_u64().is_some_and(|l| l >= 1), "{timing}");
        assert_eq!(timing["clock_resyncs"], json!(0));
        assert!(
            timing["feedback"]["margin_samples"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{timing}"
        );
        assert_eq!(timing["feedback"]["input_buffer_ticks"], json!(64));
        // ADR-8312: unacked-only redundancy and the loss-aware copy count.
        assert!(
            timing["unacked_inputs"].as_u64().is_some_and(|n| n <= 64),
            "{timing}"
        );
        assert_eq!(
            timing["input_copies"],
            json!(2),
            "a clean link counts as 1 % loss: {timing}"
        );
        let p = peers(&mut world, &json!({ "endpoint": "server" })).expect("ok");
        let row = &p["endpoints"][0]["peers"][0];
        assert!(
            row["input_feedback"]["margin_p5"]
                .as_i64()
                .is_some_and(|m| m >= 0),
            "{row}"
        );
        assert_eq!(row["input_feedback"]["inputs_late"], json!(0), "{row}");
    }

    #[test]
    fn status_peers_and_conditioner_set_round_trip() {
        let mut world = World::new();
        let mut state = NetAgentState::default();
        state.publish("server", report("server"));
        world.insert_resource(state);
        let s = status(&mut world, &json!({})).expect("ok");
        assert_eq!(s["endpoints"][0]["label"], json!("server"));
        assert_eq!(s["endpoints"][0]["tick"], json!(12));
        let p = peers(&mut world, &json!({ "endpoint": "server" })).expect("ok");
        assert_eq!(p["endpoints"][0]["peers"], json!([]));
        let err = peers(&mut world, &json!({ "endpoint": "client" })).expect_err("unknown");
        assert!(code(&err).contains("net_unknown_endpoint"));

        let ok = conditioner_set(
            &mut world,
            &json!({ "endpoint": "server", "profile": "wan", "config": { "loss": 0.25, "seed": 9 } }),
        )
        .expect("queued");
        assert_eq!(ok["queued_for"], json!(["server"]));
        assert_eq!(ok["conditioner"]["loss"], json!(0.25));
        assert_eq!(ok["conditioner"]["delay_ms"], json!(60), "profile values kept");
        let applied = world
            .resource_mut::<NetAgentState>()
            .take_conditioner_for("server")
            .expect("pending change");
        assert_eq!(applied.seed, 9);

        for (params, expect) in [
            (json!({ "profile": "moon" }), "net_unknown_profile"),
            (json!({ "config": { "lag": 5 } }), "net_bad_conditioner"),
            (json!({ "config": { "loss": "lots" } }), "net_bad_conditioner"),
            (json!({ "endpoint": "nope" }), "net_unknown_endpoint"),
        ] {
            let err = conditioner_set(&mut world, &params).expect_err("refused");
            assert!(code(&err).contains(expect), "{params}: {err:?}");
        }
    }

    /// ADR-3075: `spec` takes the one-line grammar of `--conditioner`, `config` still overrides it,
    /// and a bad spec or a spec beside a profile is `net_bad_conditioner`.
    #[test]
    fn conditioner_set_takes_a_spec_line() {
        let mut world = World::new();
        let mut state = NetAgentState::default();
        state.publish("client", report("client"));
        world.insert_resource(state);
        let ok = conditioner_set(
            &mut world,
            &json!({ "spec": "delay=40,jitter=10,loss=2%", "config": { "seed": 4 } }),
        )
        .expect("queued");
        assert_eq!(ok["conditioner"]["delay_ms"], json!(40));
        assert_eq!(ok["conditioner"]["jitter_ms"], json!(10));
        assert_eq!(ok["conditioner"]["seed"], json!(4));
        assert_eq!(ok["conditioner"]["enabled"], json!(true));
        assert_eq!(
            ok["conditions"],
            json!("delay=40 jitter=10 loss=0.020 dup=0.000 reorder=0.000 reorder_max=0 seed=4")
        );
        for (params, expect) in [
            (json!({ "spec": "delay=forever" }), "net_bad_conditioner"),
            (json!({ "spec": "wan", "profile": "wan" }), "net_bad_conditioner"),
            (json!({ "spec": 5 }), "F0002"),
        ] {
            let err = conditioner_set(&mut world, &params).expect_err("refused");
            assert!(code(&err).contains(expect), "{params}: {err:?}");
        }
    }

    #[test]
    fn net_rollback_reports_published_counters() {
        let mut world = World::new();
        let empty = rollback(&mut world, &json!({})).expect("no resource");
        assert_eq!(
            empty,
            json!({ "published": false, "replays": 0, "depth": 0, "too_old": 0 })
        );

        let mut state = NetAgentState::default();
        assert!(!state.rollback_published());
        state.publish_rollback(regia_net::RollbackCounters {
            replays: 3,
            max_depth: 4,
            too_old: 1,
        });
        world.insert_resource(state);
        let got = rollback(&mut world, &json!({})).expect("published");
        assert_eq!(
            got,
            json!({ "published": true, "replays": 3, "depth": 4, "too_old": 1 })
        );
    }
}
