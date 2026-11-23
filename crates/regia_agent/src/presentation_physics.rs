// SPDX-License-Identifier: MPL-2.0
//! `sim_bridge.presentation_stats` (`book/agents/presentation-physics.md`, ADR-8356).
//!
//! Reads the presentation-physics counters. The module is not on the lockstep closure: the
//! numbers are hulls, debris and refused writes in this process, not a sim checksum.
//!
//! Errors carry a lowercase `data.code`: `presentation_physics_unavailable` when
//! `PresentationPhysicsPlugin` is not in the app.

use regia_app::prelude::*;
use regia_framework::sim_bridge::presentation::PresentationPhysicsStats;
use serde_json::{Value, json};

use crate::protocol::{NOT_FOUND, RpcError};

const HINT: &str = "add regia_framework::sim_bridge::presentation::PresentationPhysicsPlugin";

/// `sim_bridge.presentation_stats`: live hull and debris counts, and refused dynamic writes.
pub(crate) fn presentation_stats(world: &mut World) -> Result<Value, RpcError> {
    let Some(stats) = world.get_resource::<PresentationPhysicsStats>() else {
        return Err(RpcError::new(
            NOT_FOUND,
            "presentation_physics_unavailable",
            "presentation physics is not in this app",
            HINT,
        ));
    };
    Ok(json!({
        "hull_bodies": stats.hull_bodies,
        "debris_alive": stats.debris_alive,
        "refused_writes": stats.refused_writes,
        "ground_colliders": stats.ground_colliders,
        "bursts": stats.bursts,
        "debris_spawned": stats.debris_spawned,
        "debris_skipped": stats.debris_skipped,
    }))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use regia_core::math::Vec3;
    use regia_framework::sim_bridge::presentation::{
        DebrisBurst, HullShapes, PresentationPhysicsPlugin, cube_chunk,
    };
    use regia_framework::sim_bridge::{SimId, SimKind};
    use regia_physics::{BodyKind, ColliderShape, PhysicsPlugin, RigidBody};
    use regia_scene::Transform;
    use serde_json::json;

    use super::*;
    use crate::contract::{assert_listed, assert_result_named};
    use crate::dispatch::{AgentInbox, process_requests};
    use crate::protocol::{Request, Response};
    use crate::safety::{AgentConfig, AgentPermissions};
    use crate::server::Inbound;

    fn code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or("")
            .to_string()
    }

    fn call(world: &mut World, params: Value) -> Response {
        let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        inbox_tx
            .send(Inbound {
                request: Request {
                    jsonrpc: "2.0".into(),
                    id: Some(json!(1)),
                    method: "sim_bridge.presentation_stats".into(),
                    params,
                },
                reply: reply_tx,
            })
            .expect("queue request");
        world.insert_resource(AgentInbox(inbox_rx));
        process_requests(world);
        reply_rx.try_recv().expect("dispatcher answered")
    }

    fn agent_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: 10_000,
                heavy_rate_limit_per_second: 10_000,
                ..AgentConfig::default()
            },
            source: None,
        });
        app.init_resource::<crate::safety::AuditLog>()
            .init_resource::<crate::safety::AgentSession>()
            .init_resource::<crate::dispatch::RateLimit>();
        app
    }

    fn steps(app: &mut App, n: usize) {
        for _ in 0..n {
            app.update();
        }
    }

    #[test]
    fn missing_plugin_is_presentation_physics_unavailable() {
        let mut app = agent_app();
        let err = call(app.world_mut(), json!({})).error.expect("no plugin");
        assert_eq!(code(&err), "presentation_physics_unavailable");
        assert_listed(
            "sim_bridge.presentation_stats",
            "presentation_physics_unavailable",
        );
    }

    /// End to end through the dispatcher: a hull, one debris chunk, then a refused dynamic write.
    #[test]
    fn presentation_stats_through_the_dispatcher() {
        let mut app = agent_app();
        app.add_plugins((
            PhysicsPlugin,
            PresentationPhysicsPlugin::new(HullShapes::single(
                "hull",
                ColliderShape::Box {
                    half_extents: Vec3::new(1.0, 0.25, 1.0),
                },
            )),
        ));
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed);
        let hull = app
            .world_mut()
            .spawn((
                SimId(1),
                SimKind("hull".into()),
                Transform {
                    translation: Vec3::new(0.0, 2.0, 0.0),
                    ..Transform::IDENTITY
                },
            ))
            .id();
        steps(&mut app, 3);
        let stats = call(app.world_mut(), json!({})).result.expect("hull stats");
        assert_result_named("sim_bridge.presentation_stats", &stats);
        assert_eq!(stats["hull_bodies"], json!(1));
        assert_eq!(stats["debris_alive"], json!(0));
        assert_eq!(stats["refused_writes"], json!(0));

        app.world_mut().write_message(DebrisBurst {
            origin: Vec3::new(0.0, 4.0, 0.0),
            chunks: vec![cube_chunk(Vec3::ZERO, 0.25, 5.0)],
            speed: 0.0,
        });
        steps(&mut app, 1);
        let stats = call(app.world_mut(), json!({})).result.expect("debris stats");
        assert!(
            stats["debris_alive"].as_u64().unwrap_or(0) >= 1,
            "a chunk with no sim id is alive: {stats}"
        );

        app.world_mut().entity_mut(hull).insert(RigidBody::dynamic());
        steps(&mut app, 1);
        let stats = call(app.world_mut(), json!({})).result.expect("refused stats");
        assert!(
            stats["refused_writes"].as_u64().unwrap_or(0) >= 1,
            "the dynamic sim body was refused: {stats}"
        );
        let kind = app.world().get::<RigidBody>(hull).map(|body| body.kind);
        assert_ne!(
            kind,
            Some(BodyKind::Dynamic),
            "the hull is not left dynamic: {kind:?}"
        );
    }
}
