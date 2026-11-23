// SPDX-License-Identifier: MPL-2.0
//! End-to-end smoke for the physics query surface (specs/15 § 3, specs/23 § 7): a client
//! drives a headless physics app over the real protocol — `agent.hello` advertises the
//! `physics` capability, a collider spawned through `scene.spawn` is verified with
//! `physics.raycast`, `physics.overlap` and `physics.closest_point`. No window, no GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_core::math::Vec3;
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_scene::Transform;
use serde_json::{Value, json};

/// Wall-clock guard: the whole conversation must finish well inside this.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Calls one protocol method, unwrapping errors with the method name in the message.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

#[test]
fn physics_query_agent_smoke() {
    let project = std::env::temp_dir().join(format!("regia-agent-physics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("project dir");

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, regia_physics::PhysicsPlugin))
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));

    // A body under a plain (non-physics) parent: the spec 15 § 1 warning case.
    let non_physics_parent = app.world_mut().spawn_empty().id();
    app.world_mut().spawn((
        Transform::default(),
        RigidBody::dynamic(),
        ChildOf(non_physics_parent),
    ));

    // The world the queries run against: a static ground plate (top surface at y = 0).
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("agent-physics-smoke".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&agent_dir))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };

            // Discovery: the capability is advertised and the schema lists the methods.
            let hello = call(&mut client, "agent.hello", json!({}));
            let caps = hello["capabilities"].as_array().expect("capabilities");
            assert!(caps.iter().any(|c| c == "physics"), "{hello}");
            let schema = call(&mut client, "agent.schema", json!({}));
            let names: Vec<&str> = schema["methods"]
                .as_array()
                .expect("methods")
                .iter()
                .map(|m| m["name"].as_str().expect("name"))
                .collect();
            for expected in [
                "physics.raycast",
                "physics.overlap",
                "physics.closest_point",
                "physics.parented_bodies",
            ] {
                assert!(names.contains(&expected), "schema lists {expected}: {names:?}");
            }
            let guide = call(
                &mut client,
                "agent.guide",
                json!({ "topic": "physics-parented-bodies" }),
            );
            assert!(
                guide["text"].as_str().unwrap_or("").contains("parented"),
                "{guide}"
            );

            // Let the sync chain create the rapier colliders.
            let _ = call(&mut client, "play.pause", json!({}));
            let stepped = call(&mut client, "play.step", json!({ "frames": 20 }));
            assert!(stepped["frame"].as_u64().is_some(), "stepped: {stepped}");
            assert_eq!(stepped["paused"], true, "{stepped}");

            // Raycast: straight down from 5 m hits the plate's top surface at y = 0.
            let ray = call(
                &mut client,
                "physics.raycast",
                json!({ "origin": [0.0, 5.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
            );
            let hit = ray["hit"].as_object().expect("a hit");
            assert!(
                (hit["distance"].as_f64().expect("distance") - 5.0).abs() < 1e-4,
                "{ray}"
            );
            assert_eq!(hit["normal"], json!([0.0, 1.0, 0.0]), "{ray}");
            assert_eq!(hit["point"], json!([0.0, 0.0, 0.0]), "{ray}");

            // Overlap: a sphere resting on the plate contains exactly the plate.
            let overlap = call(
                &mut client,
                "physics.overlap",
                json!({ "shape": { "kind": "sphere", "radius": 1.0 }, "position": [0.0, 0.5, 0.0] }),
            );
            assert_eq!(overlap["count"], 1, "{overlap}");

            // Closest point: 2 m above the plate projects onto its surface.
            let closest = call(
                &mut client,
                "physics.closest_point",
                json!({ "point": [0.0, 2.0, 0.0], "max_distance": 5.0 }),
            );
            assert_eq!(closest["hit"]["point"], json!([0.0, 0.0, 0.0]), "{closest}");
            assert_eq!(closest["hit"]["is_inside"], false, "{closest}");

            // The spec 15 § 1 play-world check over the wire: the body parented under a
            // non-physics parent is counted exactly once.
            let stepped = call(&mut client, "play.step", json!({ "frames": 2 }));
            assert!(stepped["frame"].as_u64().is_some(), "stepped: {stepped}");
            let parented = call(&mut client, "physics.parented_bodies", json!({}));
            assert_eq!(parented["count"], 1, "{parented}");
            assert_eq!(parented["bodies"].as_array().expect("bodies").len(), 1);
            let parent_id = parented["bodies"][0]["parent"].as_u64().expect("parent");
            assert_ne!(parent_id, 0, "the non-physics parent is named: {parented}");

            // Stats: one static body and collider, a 16-hex-digit digest that holds while
            // the app is paused (nothing steps, nothing moves).
            let stats = call(&mut client, "physics.stats", json!({}));
            // Two bodies: the static ground plate and the dynamic parented-body probe.
            assert_eq!(stats["bodies"], 2, "{stats}");
            assert_eq!(stats["fixed"], 1, "{stats}");
            assert_eq!(stats["colliders"], 1, "{stats}");
            let digest = stats["digest"].as_str().expect("digest").to_owned();
            assert_eq!(digest.len(), 16, "{stats}");
            let again = call(&mut client, "physics.stats", json!({}));
            assert_eq!(again["digest"], digest.as_str(), "{again}");

            // Read-only calls leave no provenance records.
            let changes = call(&mut client, "agent.changes", json!({}));
            assert_eq!(
                changes["records"].as_array().expect("records").len(),
                0,
                "{changes}"
            );

            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    app.finish_plugins();
    while !script_ok.load(Ordering::Relaxed) && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the scripted conversation did not complete within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Reads the bearer token the app wrote.
fn read_token(agent_dir: &std::path::Path) -> String {
    std::fs::read_to_string(agent_dir.join("token")).expect("token file")
}
