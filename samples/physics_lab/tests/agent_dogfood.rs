// SPDX-License-Identifier: MIT-0
//! Dogfood (AGENTS.md rule 8): the lab driven over the real agent protocol. A client toggles
//! `physics.debug_draw`, registers a material with `physics.material_register`, steps the app with
//! `play.step` and checks the world with `physics.stats` and `physics.raycast`. No window, no GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use physics_lab::{LabConfig, build_app_with};
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

fn read_token(agent_dir: &std::path::Path) -> String {
    std::fs::read_to_string(agent_dir.join("token")).expect("token file")
}

#[test]
fn the_physics_lab_over_the_agent_protocol() {
    let project = regia_test_utils::unique_temp_dir("physics_lab_agent");
    let (mut app, _ids) = build_app_with(&LabConfig::default(), |app| {
        app.add_plugins(MinimalPlugins);
    });
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("agent-physics-lab".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
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
            let _ = call(&mut client, "play.pause", json!({}));

            // Off by default: stepping collects nothing.
            let _ = call(&mut client, "play.step", json!({ "frames": 30 }));
            let off = call(&mut client, "physics.debug_draw", json!({}));
            assert_eq!(off["on"], false, "{off}");
            assert_eq!(off["collections"], 0, "{off}");

            // On: the next frames collect lines of every kind.
            let on = call(
                &mut client,
                "physics.debug_draw",
                json!({ "on": true, "kinds": ["contacts", "joints", "com"] }),
            );
            assert_eq!(on["on"], true, "{on}");
            let _ = call(&mut client, "play.step", json!({ "frames": 3 }));
            let state = call(&mut client, "physics.debug_draw", json!({}));
            // The app keeps updating while the sim is paused, so count frames relatively.
            assert!(state["collections"].as_u64().unwrap_or(0) >= 3, "{state}");
            assert!(state["lines"].as_u64().unwrap_or(0) > 0, "{state}");
            assert!(state["lines_emitted"].as_u64().unwrap_or(0) > 0, "{state}");
            // Off again: nothing more is collected however many frames run.
            let off_again = call(&mut client, "physics.debug_draw", json!({ "on": false }));
            assert_eq!(off_again["on"], false, "{off_again}");
            let settled = call(&mut client, "physics.debug_draw", json!({}));
            let _ = call(&mut client, "play.step", json!({ "frames": 5 }));
            let after = call(&mut client, "physics.debug_draw", json!({}));
            assert_eq!(
                after["collections"], settled["collections"],
                "{settled} vs {after}"
            );
            assert_eq!(after["lines"], 0, "{after}");

            // A bad kind is refused with its stable code.
            let bad = client.call("physics.debug_draw", json!({ "kinds": ["wheels"] }));
            assert!(
                format!("{bad:?}").contains("physics_debug_kind_unknown"),
                "{bad:?}"
            );

            // Materials: check, register, and the world still steps.
            let checked = call(
                &mut client,
                "physics.material_check",
                json!({ "ron": physics_lab::ICE_RON }),
            );
            assert_eq!(checked["name"], "ice", "{checked}");
            let registered = call(
                &mut client,
                "physics.material_register",
                json!({ "ron": "(format_version: 1, name: \"mud\", friction: 1.5)" }),
            );
            assert_eq!(registered["registered"], 3, "{registered}");

            // The tower stands: a ray down its axis hits a box top, not the floor.
            let ray = call(
                &mut client,
                "physics.raycast",
                json!({ "origin": [-6.0, 10.0, 0.0], "direction": [0.0, -1.0, 0.0] }),
            );
            let y = ray["hit"]["point"][1].as_f64().expect("hit y");
            assert!((y - 4.0).abs() < 0.1, "tower top at 4 m: {ray}");
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

/// Rule 8 for traversal: the course driven over the protocol. `scene.set_property` holds the stick,
/// `play.step` steps, `physics.character_traversal` reads the move and the probe says what is ahead.
#[test]
fn the_traversal_course_over_the_agent_protocol() {
    use physics_lab::course::{LADDER_ID, build_course_with};

    let project = regia_test_utils::unique_temp_dir("physics_lab_traversal_agent");
    let (mut app, character) = build_course_with(|app| {
        app.add_plugins(MinimalPlugins);
    });
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);

    let ok = Arc::new(AtomicBool::new(false));
    let flag = ok.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let bits = character.to_bits();
    let script = std::thread::Builder::new()
        .name("agent-traversal-course".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _)) = endpoint::read(&agent_dir)
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
            let _ = call(&mut client, "play.pause", json!({}));
            let read = |client: &mut Client| {
                let out = call(client, "physics.character_traversal", json!({ "entity": bits }));
                out["characters"][0].clone()
            };
            let stick = |client: &mut Client, x: f64| {
                let _ = call(
                    client,
                    "scene.set_property",
                    json!({ "entity": bits, "component": "CharacterInput", "path": "move_dir",
                            "value": [x, 0.0, 0.0] }),
                );
            };

            // At rest on dry ground: nothing running, and the probe finds nothing in the way.
            let _ = call(&mut client, "play.step", json!({ "frames": 10 }));
            let rest = read(&mut client);
            assert_eq!(rest["kind"], "none", "{rest}");
            let probe = call(
                &mut client,
                "physics.character_traversal_probe",
                json!({ "entity": bits, "forward": [1.0, 0.0, 0.0] }),
            );
            assert_eq!(probe["ledge"]["available"], false, "{probe}");
            assert_eq!(probe["ledge"]["reason"], "ledge_no_wall", "{probe}");
            assert_eq!(probe["water"]["available"], false, "{probe}");

            // The probe asked about the pool's deep middle answers the water.
            let probe = call(
                &mut client,
                "physics.character_traversal_probe",
                json!({ "entity": bits, "position": [12.0, -1.0, 0.0], "forward": [1.0, 0.0, 0.0] }),
            );
            assert_eq!(probe["water"]["available"], true, "{probe}");
            assert_eq!(probe["water"]["chest_under"], true, "{probe}");

            // And about the foot of the ladder: it would attach at the bottom.
            let probe = call(
                &mut client,
                "physics.character_traversal_probe",
                json!({ "entity": bits, "position": [43.4, 2.72, 0.0], "forward": [1.0, 0.0, 0.0] }),
            );
            assert_eq!(probe["ladder"]["available"], true, "{probe}");
            assert_eq!(probe["ladder"]["user_id"], LADDER_ID, "{probe}");
            assert_eq!(probe["ladder"]["attach"], "bottom", "{probe}");

            // Hold the stick: into the pool, swimming at the line.
            stick(&mut client, 1.0);
            let mut swam = false;
            for _ in 0..100 {
                let _ = call(&mut client, "play.step", json!({ "frames": 10 }));
                let row = read(&mut client);
                if row["kind"] == "swim" {
                    assert_eq!(row["mode"], "swimming", "{row}");
                    swam = true;
                    break;
                }
            }
            assert!(swam, "the character never swam");

            // The errors are the documented ones.
            let bad = client.call(
                "physics.character_traversal_probe",
                json!({ "entity": bits, "position": ["a", 0.0, 0.0] }),
            );
            assert!(bad.is_err(), "{bad:?}");
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
