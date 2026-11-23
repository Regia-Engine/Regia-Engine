// SPDX-License-Identifier: MIT-0
//! Dogfood (AGENTS.md rule 8): the lab's traffic driven over the real agent protocol. A client
//! steps the app with `play.step`, reads the four drivers with `vehicle_ai.status`, retunes one with
//! `vehicle_ai.set_params`, sends it to a lane with `vehicle_ai.drive_to` and stops it with
//! `vehicle_ai.stop`. No window, no GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nav_world_lab::traffic::build_traffic_app;
use regia::prelude::MinimalPlugins;
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

#[test]
fn the_traffic_over_the_agent_protocol() {
    let project = regia_test_utils::unique_temp_dir("nav_world_lab_traffic_agent");
    let (mut app, cars) = build_traffic_app(|app| {
        app.add_plugins(MinimalPlugins);
    })
    .expect("traffic app");
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);
    let first = cars[0].to_bits();

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("agent-traffic".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
                    && let Ok(token) = std::fs::read_to_string(agent_dir.join("token"))
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            let _ = call(&mut client, "play.pause", json!({}));
            let _ = call(&mut client, "play.step", json!({ "frames": 300 }));
            let status = call(&mut client, "vehicle_ai.status", json!({}));
            assert_eq!(status["count"], 4, "{status}");
            for driver in status["drivers"].as_array().expect("drivers") {
                // Lapping, possibly slowed by the car ahead.
                assert!(
                    driver["state"] == "driving" || driver["state"] == "following",
                    "{driver}"
                );
                assert!(driver["speed"].as_f64().unwrap_or(0.0) > 3.0, "{driver}");
                assert!(
                    driver["cross_track"].as_f64().unwrap_or(9.0).abs() < 1.5,
                    "{driver}"
                );
            }
            // Retune one car and send it to a point on the far straight.
            let tuned = call(
                &mut client,
                "vehicle_ai.set_params",
                json!({ "entity": first, "max_speed": 8.0 }),
            );
            assert_eq!(tuned["params"]["max_speed"], 8.0, "{tuned}");
            let planned = call(
                &mut client,
                "vehicle_ai.drive_to",
                json!({ "entity": first, "lane": 2, "distance": 40.0 }),
            );
            assert_eq!(planned["state"], "driving", "{planned}");
            assert_eq!(
                planned["route"].as_array().and_then(|r| r.last()),
                Some(&json!(2)),
                "{planned}"
            );
            // A bad target answers its stable code.
            let bad = client.call("vehicle_ai.drive_to", json!({ "entity": first, "lane": 99 }));
            assert!(format!("{bad:?}").contains("vehicle_ai_unknown_lane"), "{bad:?}");
            // Stop it: it brakes to a halt and holds.
            let stopped = call(&mut client, "vehicle_ai.stop", json!({ "entity": first }));
            assert_eq!(stopped["state"], "idle", "{stopped}");
            let _ = call(&mut client, "play.step", json!({ "frames": 240 }));
            let one = call(&mut client, "vehicle_ai.status", json!({ "entity": first }));
            let row = &one["drivers"][0];
            assert_eq!(row["state"], "idle", "{one}");
            assert!(row["speed"].as_f64().unwrap_or(9.0).abs() < 0.3, "{one}");
            flag.store(true, Ordering::SeqCst);
            let _ = client.call("agent.shutdown", json!({}));
        })
        .expect("script thread");

    app.finish_plugins();
    for _ in 0..1_000_000 {
        app.update();
        if ok.load(Ordering::SeqCst) || script.is_finished() {
            break;
        }
    }
    let _ = script.join();
    assert!(ok.load(Ordering::SeqCst), "the agent script did not finish");
    let _ = std::fs::remove_dir_all(&project);
}
