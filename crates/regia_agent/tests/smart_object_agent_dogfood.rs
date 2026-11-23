// SPDX-License-Identifier: MPL-2.0
//! Spec 39 § 11.11 dogfood over the real protocol: a client claims a smart-object slot with a
//! priority, a second agent preempts it at a higher one, the first handle goes stale, and
//! `smartobject.messages` reports the `lost` record (lane spec-17-39, ADR-5636). Headless: no
//! window, no GPU. No wall-clock budget: the app updates until the script thread ends, and the
//! thread's own assertions decide the result.

use std::time::Duration;

use regia::prelude::*;
use regia_agent::client::{Client, ClientError};
use regia_agent::endpoint;
use regia_ai::{SmartObject, SmartObjectPlugin, SmartObjectSlot};
use regia_app::prelude::ChildOf;
use regia_core::math::{Affine3A, Vec3};
use regia_scene::GlobalTransform;
use serde_json::{Value, json};

/// Calls one protocol method, unwrapping errors with the method name in the message.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

/// The `data.reason` of a refused call.
fn refusal(client: &mut Client, method: &str, params: Value) -> String {
    match client.call(method, params) {
        Ok(value) => panic!("{method} should be refused, answered {value}"),
        Err(ClientError::Rpc(error)) => error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        Err(other) => panic!("{method}: {other:?}"),
    }
}

#[test]
fn a_preempted_claim_is_reported_over_the_protocol() {
    let project = regia_test_utils::unique_temp_dir("agent-smartobject-dogfood");
    std::fs::create_dir_all(&project).expect("project dir");

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SmartObjectPlugin))
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    let object = app.world_mut().spawn(SmartObject::default()).id();
    let slot = app
        .world_mut()
        .spawn((
            SmartObjectSlot::new("sit"),
            GlobalTransform(Affine3A::from_translation(Vec3::new(1.0, 0.0, 0.0))),
            ChildOf(object),
        ))
        .id()
        .to_bits();
    let first = app.world_mut().spawn_empty().id().to_bits();
    let second = app.world_mut().spawn_empty().id().to_bits();

    // One whole frame before the client starts: `SmartObjectPlugin` builds the index the search
    // answers from in `PostUpdate`, and the agent serves calls in `PreUpdate`, so a call served in
    // the app's first frame sees no slot yet. The M5 run's client got in that early and its search
    // found no hits (ADR-7386).
    app.finish_plugins();
    app.update();

    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("agent-smartobject-dogfood".into())
        .spawn(move || {
            // The app writes its endpoint on its first updates; poll for it (bounded).
            let mut client = None;
            for _ in 0..50_000_000u64 {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(token) = std::fs::read_to_string(agent_dir.join("token"))
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    client = Some(connected);
                    break;
                }
                std::thread::yield_now();
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };

            // The slot is listed and free; a search finds it.
            let hits = call(
                &mut client,
                "smartobject.search",
                json!({ "region": { "center": [0, 0, 0], "radius": 10 } }),
            );
            assert_eq!(hits["hits"][0]["slot"], slot, "{hits}");
            assert_eq!(hits["hits"][0]["status"], "free", "{hits}");

            // A low-priority claim, refused to an equal one, taken by an urgent one.
            let low = call(
                &mut client,
                "smartobject.claim",
                json!({ "slot": slot, "agent": first, "priority": "low" }),
            );
            assert_eq!(low["claimed"], true, "{low}");
            let handle = low["handle"].clone();
            let equal = call(
                &mut client,
                "smartobject.claim",
                json!({ "slot": slot, "agent": second, "priority": "low" }),
            );
            assert_eq!(equal["claimed"], false, "{equal}");
            assert_eq!(equal["reason"], "claimed", "{equal}");
            let urgent = call(
                &mut client,
                "smartobject.claim",
                json!({ "slot": slot, "agent": second, "priority": "urgent" }),
            );
            assert_eq!(urgent["claimed"], true, "{urgent}");

            // The first handle is stale; the second agent's handle releases only for it.
            assert_eq!(
                refusal(&mut client, "smartobject.release", json!({ "handle": handle })),
                "stale_handle"
            );
            assert_eq!(
                refusal(
                    &mut client,
                    "smartobject.release",
                    json!({ "handle": urgent["handle"], "agent": first })
                ),
                "not_holder"
            );

            // The `lost` record reaches the log once PostUpdate has flushed it.
            let mut lost = Vec::new();
            for _ in 0..1_000 {
                let log = call(&mut client, "smartobject.messages", json!({}));
                lost = log["messages"]
                    .as_array()
                    .expect("messages")
                    .iter()
                    .filter(|m| m["kind"] == "lost")
                    .cloned()
                    .collect();
                if !lost.is_empty() {
                    break;
                }
            }
            assert_eq!(lost.len(), 1, "{lost:?}");
            assert_eq!(lost[0]["reason"], "preempted", "{lost:?}");
            assert_eq!(lost[0]["holder"], first, "{lost:?}");
            assert_eq!(lost[0]["handle"], handle, "{lost:?}");

            let released = call(
                &mut client,
                "smartobject.release",
                json!({ "handle": urgent["handle"], "agent": second }),
            );
            assert_eq!(released["released"], true, "{released}");
        })
        .expect("client thread");

    while !script.is_finished() {
        app.update();
    }
    script.join().expect("the scripted conversation passed");
    let _ = std::fs::remove_dir_all(&project);
}
