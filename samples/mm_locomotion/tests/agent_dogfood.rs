// SPDX-License-Identifier: MIT-0
//! Rule 8 dogfooding (ADR-2409): an agent reads and steers the sample's motion matching over the
//! real agent protocol. The app runs headless at a fixed 1/30 s step with the agent server; a
//! script thread finds the hero's database by asset path, waits until the course makes the hero
//! run, reads the candidate report, then forbids the `run` tag and watches the matcher leave it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mm_locomotion::{DATABASE_PATH, headless_app};
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};

/// Hang guard only: the conversation takes a few hundred frames.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(180);

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

#[test]
fn an_agent_reads_the_database_by_path_and_steers_the_matcher() {
    let project = regia_test_utils::temp::unique_temp_dir("mm-locomotion-agent");
    let mut app = headless_app();
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("mm-locomotion-agent".into())
        .spawn(move || {
            let mut agent = None;
            for _ in 0..3000 {
                if let Ok((advertised, token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    agent = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut agent) = agent else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            // The registry names the database and the hero that uses it.
            let mut list = Value::Null;
            for _ in 0..500 {
                list = call(&mut agent, "anim.motion_match_databases", json!({}));
                if list["databases"][0]["entities"]
                    .as_array()
                    .is_some_and(|e| !e.is_empty())
                {
                    break;
                }
            }
            assert_eq!(list["databases"][0]["asset"], DATABASE_PATH, "{list}");
            let hero = list["databases"][0]["entities"][0].as_u64().expect("hero entity");
            let db = call(
                &mut agent,
                "anim.motion_match_database",
                json!({ "database": DATABASE_PATH }),
            );
            assert_eq!(db["asset"], DATABASE_PATH, "{db}");
            assert_eq!(db["clips"].as_array().map(Vec::len), Some(6), "{db}");
            assert_eq!(db["playable"], true, "{db}");
            assert_eq!(db["tags"]["run"].as_u64().map(|n| n > 0), Some(true), "{db}");
            let by_entity = call(
                &mut agent,
                "anim.motion_match_database",
                json!({ "entity": hero }),
            );
            assert_eq!(by_entity["content_hash"], db["content_hash"]);

            // The course reaches its run leg: the matcher plays `run`.
            let mut running = false;
            for _ in 0..4000 {
                let read = call(&mut agent, "anim.motion_match", json!({ "entity": hero }));
                if read["playback"]["clip_name"] == "run" {
                    running = true;
                    break;
                }
            }
            assert!(running, "the hero ran");
            let report = call(
                &mut agent,
                "anim.motion_match",
                json!({ "entity": hero, "candidates": 4 }),
            );
            let rows = report["candidates"].as_array().expect("candidates");
            assert!(!rows.is_empty(), "{report}");
            assert!(
                rows.iter()
                    .all(|r| r["groups"]["trajectory_position"].as_f64().is_some())
            );

            // Forbid `run`: the next searches leave it.
            let set = call(
                &mut agent,
                "anim.motion_match_set",
                json!({ "entity": hero, "filter": { "forbid": ["run"] }, "search": "now" }),
            );
            assert_eq!(set["forbid"], json!(["run"]), "{set}");
            let mut left = false;
            for _ in 0..4000 {
                let read = call(&mut agent, "anim.motion_match", json!({ "entity": hero }));
                if read["playback"]["clip_name"] != "run" && read["playback"]["clip_name"].is_string() {
                    left = true;
                    break;
                }
            }
            assert!(left, "a forbidden tag is left at the next search");

            // The path comes from the predictor (ADR-2858): read it, then steer it by velocity.
            let predicted = call(&mut agent, "anim.mm_predict", json!({ "entity": hero }));
            assert_eq!(predicted["drives_matcher"], true, "{predicted}");
            assert_eq!(
                predicted["trajectory"].as_array().map(Vec::len),
                Some(3),
                "{predicted}"
            );
            let read = call(&mut agent, "anim.motion_match", json!({ "entity": hero }));
            assert_eq!(read["predictor"], true, "{read}");
            let set = call(
                &mut agent,
                "anim.motion_match_set",
                json!({ "entity": hero, "predict": { "facing": "aim", "aim_yaw": 0.0 } }),
            );
            assert!(
                set["applied"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|n| n == "predict")),
                "{set}"
            );
            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");

    app.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("the script passed");
    assert!(done.load(Ordering::Relaxed), "the conversation did not finish");
    let _ = std::fs::remove_dir_all(&project);
}
