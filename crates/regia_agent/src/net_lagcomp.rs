// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of spec 75 (lag compensation; build record spec 90,
//! `book/agents/net-lag-compensation.md`, ADR-1752): `lagcomp.stats`, `lagcomp.history`,
//! `lagcomp.raycast`, `lagcomp.last_queries`, `lagcomp.echoes`, and `lagcomp.rewound_shapes`
//! (dev builds, ADR-2551).
//!
//! Errors are `F0002` with `data.reason` set to one of: `net_not_running`, `net_not_server`,
//! `net_not_client`, `lagcomp_not_installed`, `peer_unknown`, `entity_not_found`,
//! `entity_not_lag_compensated`, `tick_out_of_history`, `lagcomp_dev_only`.

use regia_app::prelude::*;
use regia_core::math::{Ray, Vec3};
use regia_net::NetPlayerId;
use regia_net::lagcomp::echo::{EchoQueryError, echoes_report};
use regia_net::lagcomp::{
    ClaimInvalid, HitFilter, HitReport, LagCompError, LagCompHistory, LagCompQueryError, LagCompWindows,
    LagCompensation, history_report, last_queries_report, rewound_shapes_report, stats_report,
};
use regia_net::model::{NetTickFrac, PeerId};
use regia_net::runtime::NetNode;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

fn err(e: LagCompQueryError) -> RpcError {
    let (message, hint) = match e {
        LagCompQueryError::Query(LagCompError::NotServer) => (
            "lag compensation runs on the server; this process is a client",
            "call it on the server, the listen host or an offline process",
        ),
        LagCompQueryError::Query(LagCompError::NotInstalled) => (
            "lag compensation is not installed on this server",
            "add regia_net::LagCompPlugin (or call regia_net::lagcomp::install_lag_compensation)",
        ),
        LagCompQueryError::Query(LagCompError::PeerUnknown) => (
            "no connected peer has that id",
            "net.status lists the connected peers",
        ),
        LagCompQueryError::Query(LagCompError::TickOutOfHistory) => (
            "the tick is outside the recorded history",
            "lagcomp.stats gives the recorded range; pass a tick inside it, or omit `tick`",
        ),
        LagCompQueryError::NotLagCompensated => (
            "the entity has no recorded hit-shape history",
            "give it HitShapes on a replicated entity (without NoLagCompensation) and let a tick pass",
        ),
    };
    f0002(e.code(), message, hint)
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

fn vec3(params: &Value, key: &str) -> Result<Vec3, RpcError> {
    let bad = || RpcError::invalid_params(format!("`{key}` must be [x, y, z] (numbers)"));
    let a = params.get(key).and_then(Value::as_array).ok_or_else(bad)?;
    if a.len() != 3 {
        return Err(bad());
    }
    let n = |i: usize| a[i].as_f64().map(|v| v as f32).filter(|v| v.is_finite());
    match (n(0), n(1), n(2)) {
        (Some(x), Some(y), Some(z)) => Ok(Vec3::new(x, y, z)),
        _ => Err(bad()),
    }
}

/// `lagcomp.stats`: the history's size and range, query and clamp rates, the mean rewind, claim
/// verdicts by reason, the hooks installed, and per-shooter counters.
pub(crate) fn stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let s = stats_report(world).map_err(|e| err(e.into()))?;
    let mut v = serde_json::to_value(&s).map_err(|e| RpcError::internal(format!("lagcomp.stats: {e}")))?;
    let shooters: Vec<Value> = world
        .get_resource::<LagCompHistory>()
        .map(|h| h.all_shooter_counters())
        .unwrap_or_default()
        .into_iter()
        .map(|(id, c)| {
            let invalid: serde_json::Map<String, Value> = ClaimInvalid::ALL
                .iter()
                .map(|r| (r.code().to_string(), json!(c.invalid(*r))))
                .collect();
            json!({
                "peer": id.peer.0,
                "local": id.local,
                "queries": c.queries,
                "clamped": c.clamped,
                "claims_valid": c.claims_valid,
                "claims_invalid_by_reason": invalid,
            })
        })
        .collect();
    v["shooters"] = Value::Array(shooters);
    // Each shooter's measured rewind window and its checked view ticks (spec 120 § 9.6).
    let windows = world
        .get_resource::<LagCompWindows>()
        .map(LagCompWindows::all)
        .unwrap_or_default();
    v["windows"] =
        serde_json::to_value(&windows).map_err(|e| RpcError::internal(format!("lagcomp.stats: {e}")))?;
    Ok(v)
}

/// `lagcomp.history`: every recorded tick of one entity with each shape's tag and world pose.
pub(crate) fn history(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let e = Entity::try_from_bits(bits);
    // A despawned entity keeps its history until it ages out, so only an entity that neither
    // exists nor has history is "not found".
    let ticks = match e {
        Some(e) => match history_report(world, e) {
            Ok(t) => t,
            Err(LagCompQueryError::NotLagCompensated) if world.get_entity(e).is_err() => {
                return Err(f0002(
                    "entity_not_found",
                    format!("entity {bits} does not exist and has no history"),
                    "use net.replicated or scene.query_entities",
                ));
            }
            Err(x) => return Err(err(x)),
        },
        None => {
            stats_report(world).map_err(|e| err(e.into()))?;
            return Err(f0002(
                "entity_not_found",
                format!("entity {bits} does not exist"),
                "use net.replicated or scene.query_entities",
            ));
        }
    };
    Ok(json!({ "entity": bits, "ticks": ticks }))
}

/// `lagcomp.raycast`: a rewound raycast for a peer's player (at its view tick, clamped), or at an
/// explicit `tick` without shooter exclusion. The query is logged and counted like a game's.
/// Development builds only (spec 75).
pub(crate) fn raycast(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    raycast_gated(world, params, cfg!(debug_assertions))
}

fn raycast_gated(world: &mut World, params: &Value, allowed: bool) -> Result<Value, RpcError> {
    running(world)?;
    dev_gate("lagcomp.raycast", allowed)?;
    let lc = LagCompensation::new(world).map_err(|e| err(e.into()))?;
    let peer = crate::params::opt_u64(params, "peer")?
        .and_then(|p| u32::try_from(p).ok())
        .map(PeerId)
        .ok_or_else(|| RpcError::invalid_params("`peer` (peer id) is required"))?;
    let local = crate::params::opt_u64(params, "local")?.map_or(Ok(0), |l| {
        u8::try_from(l).map_err(|_| RpcError::invalid_params("`local` is 0..=7"))
    })?;
    let origin = vec3(params, "origin")?;
    let direction = vec3(params, "direction")?;
    let max_dist = crate::params::opt_f64(params, "max_dist")?.map_or(1000.0, |d| d as f32);
    let shooter = NetPlayerId { peer, local };
    let rewound = lc.rewind_tick(shooter).map_err(|e| err(e.into()))?;
    let ray = Ray::new(origin, direction);
    let filter = HitFilter::default();
    let (hit, tick) = match crate::params::opt_f64(params, "tick")? {
        Some(t) => {
            let view = lc.at_tick(NetTickFrac(t)).map_err(|e| err(e.into()))?;
            (view.raycast(ray, max_dist, &filter), t)
        }
        None => (lc.raycast(shooter, ray, max_dist, &filter), rewound.0),
    };
    let hit = hit.map_err(|e| err(e.into()))?;
    Ok(json!({
        "hit": hit.as_ref().map(HitReport::of),
        "rewound_tick": tick,
    }))
}

/// `lagcomp.last_queries`: the newest queries (at most 16), oldest first, with shooter, ticks,
/// result and the rewound shapes of the entity hit.
pub(crate) fn last_queries(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(16) as usize;
    let q = last_queries_report(world, limit).map_err(|e| err(e.into()))?;
    Ok(json!({ "queries": q }))
}

/// `lagcomp.echoes`: the rewound shapes the server echoed to this client (at most 16, oldest
/// first), with `lag_compensation.debug_echo` on in a dev build.
pub(crate) fn echoes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    running(world)?;
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(16) as usize;
    let echoes = echoes_report(world, limit).map_err(|e| {
        let (message, hint) = match e {
            EchoQueryError::NotClient => (
                "the debug echo arrives on the shooting client; this process is not a client",
                "call it on a client, or use lagcomp.last_queries on the server",
            ),
            EchoQueryError::NotInstalled => (
                "the debug echo is not installed on this client",
                "add regia_net::lagcomp::LagCompEchoPlugin on every process",
            ),
        };
        f0002(e.code(), message, hint)
    })?;
    Ok(json!({ "echoes": echoes }))
}

/// Refuses a dev-build-only method in a shipping build (`allowed` is the build's answer, a
/// parameter so a test sees the refusal).
fn dev_gate(method: &str, allowed: bool) -> Result<(), RpcError> {
    if allowed {
        return Ok(());
    }
    Err(f0002(
        "lagcomp_dev_only",
        format!("{method} is available in development builds only"),
        "run a development build of the server",
    ))
}

/// `lagcomp.rewound_shapes` (dev builds): the hit shapes of one entity posed at a server tick as
/// a rewound query tests them; `source` says whether they came from the recorded animation state
/// (`anim_state`, exact-tick joints, ADR-2551) or from recorded shape poses. Also reports the
/// history's animation-state counters.
pub(crate) fn rewound_shapes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    rewound_shapes_gated(world, params, cfg!(debug_assertions))
}

fn rewound_shapes_gated(world: &mut World, params: &Value, allowed: bool) -> Result<Value, RpcError> {
    running(world)?;
    dev_gate("lagcomp.rewound_shapes", allowed)?;
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let bad_tick = || RpcError::invalid_params("`tick` (fractional server tick) is required");
    let tick = params.get("tick").and_then(Value::as_f64).ok_or_else(bad_tick)?;
    if !tick.is_finite() {
        return Err(bad_tick());
    }
    let not_found = || {
        f0002(
            "entity_not_found",
            format!("entity {bits} does not exist and has no history"),
            "use net.replicated or scene.query_entities",
        )
    };
    let Some(e) = Entity::try_from_bits(bits) else {
        stats_report(world).map_err(|e| err(e.into()))?;
        return Err(not_found());
    };
    let shapes = match rewound_shapes_report(world, e, tick) {
        Ok(r) => r,
        Err(LagCompQueryError::NotLagCompensated) if world.get_entity(e).is_err() => return Err(not_found()),
        Err(x) => return Err(err(x)),
    };
    let mut v = serde_json::to_value(&shapes)
        .map_err(|e| RpcError::internal(format!("lagcomp.rewound_shapes: {e}")))?;
    if let Some(h) = world.get_resource::<LagCompHistory>() {
        let (poses, failed, outside) = h.anim_counters();
        v["anim"] = json!({
            "entities": h.anim_entities(),
            "poses": poses,
            "failed": failed,
            "reach_exceeded": outside,
            "fallbacks": h.anim_fallbacks(),
            "history_bytes": h.entity_bytes(e),
        });
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::lagcomp::{install_lag_compensation, lagcomp_record};
    use regia_net::model::{NetId, NetMode, NetPlayer, NetTick};
    use regia_net::runtime::{ensure_resources, server_add_peer};
    use regia_net::{HitShape, HitShapeKind, HitShapes, NetViewTick, PoseSource};
    use regia_scene::transform::Transform;

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    fn server() -> (World, Entity) {
        let mut w = World::new();
        w.insert_resource(NetMode::DedicatedServer);
        ensure_resources(&mut w);
        install_lag_compensation(&mut w);
        server_add_peer(&mut w, PeerId(1));
        let target = w
            .spawn((
                NetId(1),
                Transform::from_translation(Vec3::new(0.0, 0.0, 10.0)),
                HitShapes {
                    shapes: vec![HitShape::new(HitShapeKind::Sphere { radius: 0.5 }, "body")],
                    pose_source: PoseSource::Root,
                },
            ))
            .id();
        w.spawn((
            NetPlayer {
                id: NetPlayerId {
                    peer: PeerId(1),
                    local: 0,
                },
            },
            NetViewTick {
                tick: 8.0,
                delay_ms: 50.0,
            },
        ));
        for t in 0..=10 {
            w.resource_mut::<NetNode>().tick = NetTick(t);
            lagcomp_record(&mut w);
        }
        w.resource_mut::<NetNode>().tick = NetTick(11);
        (w, target)
    }

    /// Spec 75 § 13 test 12: every method round-trips and each error code is produced.
    #[test]
    fn lagcomp_methods_round_trip_and_every_reason() {
        let (mut w, target) = server();
        let ray = json!({ "peer": 1, "origin": [0.0, 0.0, 0.0], "direction": [0.0, 0.0, 1.0] });
        let r = raycast(&mut w, &ray).expect("raycast");
        assert_eq!(r["rewound_tick"], json!(8.0));
        assert_eq!(r["hit"]["tag"], json!("body"));
        assert_eq!(r["hit"]["net_id"], json!(1));
        let at = json!({ "peer": 1, "origin": [3.0, 0.0, 0.0], "direction": [0.0, 0.0, 1.0], "tick": 9.5 });
        let r = raycast(&mut w, &at).expect("raycast at tick");
        assert_eq!(r["hit"], Value::Null);
        assert_eq!(r["rewound_tick"], json!(9.5));

        let s = stats(&mut w, &json!({})).expect("stats");
        assert_eq!(s["entities"], json!(1));
        assert_eq!(s["ticks_kept"], json!(11));
        assert_eq!(s["range"], json!([0, 10]));
        assert_eq!(s["enabled"], json!(true));
        assert_eq!(s["occlusion"], json!(false));
        assert_eq!(s["claims_invalid_by_reason"]["missed_shape"], json!(0));
        assert_eq!(s["shooters"][0]["queries"], json!(1));
        // The view tick was set directly, not consumed from a command: no window yet.
        assert_eq!(s["windows"], json!([]));

        let h = history(&mut w, &json!({ "entity": target.to_bits() })).expect("history");
        assert_eq!(h["ticks"].as_array().map(Vec::len), Some(11));
        assert_eq!(h["ticks"][0]["shapes"][0]["tag"], json!("body"));
        assert_eq!(h["ticks"][0]["shapes"][0]["position"], json!([0.0, 0.0, 10.0]));

        let q = last_queries(&mut w, &json!({ "limit": 1 })).expect("last queries");
        assert_eq!(q["queries"].as_array().map(Vec::len), Some(1));
        assert_eq!(q["queries"][0]["kind"], json!("raycast"));
        assert_eq!(q["queries"][0]["rewound_tick"], json!(9.5));

        // Every error reason.
        let bad = |w: &mut World, m: fn(&mut World, &Value) -> Result<Value, RpcError>, p: Value| {
            reason(&m(w, &p).expect_err("error"))
        };
        let other = w.spawn(Transform::default()).id();
        assert_eq!(
            bad(&mut w, history, json!({ "entity": other.to_bits() })).as_deref(),
            Some("entity_not_lag_compensated")
        );
        let gone = w.spawn(Transform::default()).id();
        w.despawn(gone);
        assert_eq!(
            bad(&mut w, history, json!({ "entity": gone.to_bits() })).as_deref(),
            Some("entity_not_found")
        );
        assert_eq!(
            bad(
                &mut w,
                raycast,
                json!({ "peer": 7, "origin": [0, 0, 0], "direction": [0, 0, 1] })
            )
            .as_deref(),
            Some("peer_unknown")
        );
        assert_eq!(
            bad(
                &mut w,
                raycast,
                json!({ "peer": 1, "origin": [0, 0, 0], "direction": [0, 0, 1], "tick": 400.0 })
            )
            .as_deref(),
            Some("tick_out_of_history")
        );
        let mut bare = World::new();
        bare.insert_resource(NetMode::DedicatedServer);
        assert_eq!(
            bad(&mut bare, stats, json!({})).as_deref(),
            Some("net_not_running")
        );
        assert_eq!(
            bad(&mut w, echoes, json!({})).as_deref(),
            Some("net_not_client"),
            "the echo arrives on clients"
        );
        ensure_resources(&mut bare);
        assert_eq!(
            bad(&mut bare, stats, json!({})).as_deref(),
            Some("lagcomp_not_installed")
        );
        let mut client = World::new();
        client.insert_resource(NetMode::Client);
        ensure_resources(&mut client);
        install_lag_compensation(&mut client);
        assert_eq!(
            bad(&mut client, last_queries, json!({})).as_deref(),
            Some("net_not_server")
        );
        // lagcomp.rewound_shapes: the shapes at a tick, every reason it adds or shares.
        let r = rewound_shapes_gated(&mut w, &json!({ "entity": target.to_bits(), "tick": 9.5 }), true)
            .expect("rewound shapes");
        assert_eq!(r["source"], json!("recorded"));
        assert_eq!(r["tick"], json!(9.5));
        assert_eq!(r["alive"], json!(true));
        assert_eq!(r["shapes"][0]["tag"], json!("body"));
        assert_eq!(r["shapes"][0]["position"], json!([0.0, 0.0, 10.0]));
        assert_eq!(r["anim"]["entities"], json!(0));
        assert!(r["anim"]["history_bytes"].as_u64().is_some_and(|b| b > 0));
        let gated = |w: &mut World, p: Value, allowed: bool| {
            reason(&rewound_shapes_gated(w, &p, allowed).expect_err("error"))
        };
        assert_eq!(
            gated(&mut w, json!({ "entity": target.to_bits(), "tick": 9.0 }), false).as_deref(),
            Some("lagcomp_dev_only")
        );
        // Spec 75: `lagcomp.raycast` is a dev-build read, the same gate as rewound shapes.
        assert_eq!(
            reason(
                &raycast_gated(
                    &mut w,
                    &json!({ "peer": 1, "origin": [0.0, 0.0, 0.0], "direction": [0.0, 0.0, 1.0] }),
                    false,
                )
                .expect_err("shipping raycast")
            )
            .as_deref(),
            Some("lagcomp_dev_only")
        );
        assert_eq!(
            gated(&mut w, json!({ "entity": target.to_bits(), "tick": 400.0 }), true).as_deref(),
            Some("tick_out_of_history")
        );
        assert_eq!(
            gated(&mut w, json!({ "entity": other.to_bits(), "tick": 9.0 }), true).as_deref(),
            Some("entity_not_lag_compensated")
        );
        assert_eq!(
            gated(&mut w, json!({ "entity": gone.to_bits(), "tick": 9.0 }), true).as_deref(),
            Some("entity_not_found")
        );
        assert_eq!(
            gated(
                &mut client,
                json!({ "entity": target.to_bits(), "tick": 9.0 }),
                true
            )
            .as_deref(),
            Some("net_not_server")
        );
        assert!(
            rewound_shapes_gated(&mut w, &json!({ "entity": target.to_bits() }), true)
                .is_err_and(|e| reason(&e).is_none())
        );
        // Malformed params are plain invalid-params errors.
        let e = raycast(
            &mut w,
            &json!({ "peer": 1, "origin": [0, 0], "direction": [0, 0, 1] }),
        )
        .expect_err("bad origin");
        assert!(reason(&e).is_none());
    }

    /// Spec 90 § 6: `lagcomp.echoes` on a client returns the newest `limit` echoes, oldest first,
    /// with the rewound shapes; a client without `LagCompEchoPlugin` gets `lagcomp_not_installed`.
    #[test]
    fn lagcomp_echoes_on_a_client() {
        use regia_net::lagcomp::{EchoShape, LagCompEcho, LagCompEchoLog};

        let mut client = World::new();
        client.insert_resource(NetMode::Client);
        ensure_resources(&mut client);
        assert_eq!(
            reason(&echoes(&mut client, &json!({})).expect_err("no echo log")).as_deref(),
            Some("lagcomp_not_installed")
        );

        let mut log = LagCompEchoLog::default();
        for tick in [20u32, 21, 22] {
            log.push(LagCompEcho {
                server_tick: tick,
                rewound_tick: f64::from(tick) - 6.5,
                origin: Vec3::new(0.0, 1.0, 0.0),
                direction: Vec3::Z,
                shapes: vec![EchoShape {
                    tag: "head".into(),
                    kind: HitShapeKind::Sphere { radius: 0.25 },
                    position: Vec3::new(0.0, 1.5, tick as f32),
                    rotation: regia_core::math::Quat::IDENTITY,
                }],
            });
        }
        client.insert_resource(log);

        let all = echoes(&mut client, &json!({})).expect("echoes");
        assert_eq!(all["echoes"].as_array().map(Vec::len), Some(3));
        let newest = echoes(&mut client, &json!({ "limit": 2 })).expect("limited");
        let list = newest["echoes"].as_array().cloned().unwrap_or_default();
        assert_eq!(list.len(), 2, "the newest two");
        assert_eq!(list[0]["server_tick"], json!(21), "oldest first");
        assert_eq!(list[1]["server_tick"], json!(22));
        assert_eq!(list[1]["rewound_tick"], json!(15.5));
        assert_eq!(list[1]["origin"], json!([0.0, 1.0, 0.0]));
        assert_eq!(list[1]["direction"], json!([0.0, 0.0, 1.0]));
        assert_eq!(list[1]["shapes"][0]["tag"], json!("head"));
        assert_eq!(list[1]["shapes"][0]["kind"], json!("sphere"));
        assert_eq!(list[1]["shapes"][0]["size"], json!([0.25]));
        assert_eq!(list[1]["shapes"][0]["position"], json!([0.0, 1.5, 22.0]));
        assert!(
            echoes(&mut client, &json!({ "limit": "two" })).is_err_and(|e| reason(&e).is_none()),
            "a wrong-typed limit is a plain invalid-params error"
        );
    }
}
