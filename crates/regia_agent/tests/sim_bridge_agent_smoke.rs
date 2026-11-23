// SPDX-License-Identifier: MPL-2.0
//! End-to-end smoke against the `sim_bridge` sample (M11 acceptance, specs/23 § 7): a client
//! drives a headless app through the whole protocol surface — discovery, scene edits with
//! provenance, event taps, live settings — and verifies gameplay with `play.wait_until` and
//! `play.expect`. No window, no GPU: the app runs with `MinimalPlugins` and manual frames, like
//! the sample's own headless tests.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use regia_scene::Transform;
use serde_json::{Value, json};
use sim_bridge::SimBridgeSample;

/// Wall-clock guard: the whole conversation must finish well inside this.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(90);

/// Builds the headless sim_bridge app with the agent server on a temp project directory.
fn app(project: PathBuf) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    })
    .insert_resource(SettingsPath(None))
    .add_plugins((MinimalPlugins, SimBridgeSample))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project),
    })
    .add_plugins(regia_agent::AgentPlugin)
    .register_type::<Transform>()
    .register_type::<Name>()
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

/// Calls one protocol method, unwrapping errors with the method name in the message.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

#[test]
fn sim_bridge_agent_smoke_wait_until_and_expect() {
    let project = std::env::temp_dir().join(format!("regia-agent-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("project dir");

    // The log ring (logs.read) only fills when the RingLayer subscriber is installed; in a test
    // binary that is usually free to do. When it is not (a subscriber already set), the logs
    // assertion is skipped rather than the smoke failing on harness plumbing.
    let logs_installed = regia_core::log::init(regia_core::log::LogConfig::default()).is_ok();
    tracing::info!(target: "regia_agent", "smoke: logging started");

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("agent-smoke-client".into())
        .spawn(move || {
            // Wait for the endpoint, then run the scripted conversation.
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&script_project))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };

            // -- Discovery ------------------------------------------------------
            let hello = call(
                &mut client,
                "agent.hello",
                json!({
                    "client": "smoke-test", "model": "glm"
                }),
            );
            assert_eq!(hello["protocol"], "0.2", "{hello}");
            let methods = hello["methods"].as_array().expect("methods");
            assert!(methods.len() >= 40, "the full M11 surface: {methods:?}");
            let caps = hello["capabilities"].as_array().expect("capabilities");
            assert!(caps.iter().any(|c| c == "sim_bridge"), "{hello}");

            let info = call(&mut client, "project.info", json!({}));
            let subsystems = info["subsystems"].as_array().expect("subsystems");
            assert!(subsystems.iter().any(|s| s == "sim_bridge"), "{info}");
            assert!(subsystems.iter().any(|s| s == "settings"), "{info}");
            assert!(
                info["audit_log"].as_str().is_some(),
                "the audit log is known: {info}"
            );

            let schema = call(&mut client, "agent.schema", json!({}));
            assert_eq!(schema["protocol"], "0.2");
            assert_eq!(schema["methods"].as_array().map(Vec::len), Some(methods.len()));

            let guide = call(&mut client, "agent.guide", json!({ "topic": "testing" }));
            assert!(guide["text"].as_str().unwrap_or("").contains("wait_until"));

            let transform_schema = call(&mut client, "types.describe", json!({ "type": "Transform" }));
            assert!(transform_schema["schema"]["properties"]["translation"].is_object());

            // -- wait_until: the three discs appear and hover ----------------------
            // (The app may not have presented them when the client first connects, so the
            // condition is polled until it holds.)
            let presented = call(
                &mut client,
                "play.wait_until",
                json!({
                    "condition": {
                        "entity": { "component": "SimId" }, "component": "Transform",
                        "path": "translation.y", "op": "eq", "value": 1.5
                    },
                    "timeout_ms": 30_000
                }),
            );
            assert_eq!(presented["satisfied"], true, "discs presented: {presented}");

            let discs = call(
                &mut client,
                "scene.query_entities",
                json!({ "component": "SimId", "limit": 10 }),
            );
            assert_eq!(discs["count"], 3, "the hover sim presents three discs: {discs}");

            // -- expect: assertions over the running sim ---------------------------
            let expect = call(
                &mut client,
                "play.expect",
                json!({
                    "assertions": [
                        { "name": "discs hover at 1.5 m",
                          "condition": { "entity": { "component": "SimId" }, "component": "Transform",
                                         "path": "translation.y", "op": "eq", "value": 1.5 } },
                        { "name": "time advances",
                          "condition": { "resource": "Time", "path": "frame", "op": "gte", "value": 1 } },
                    ]
                }),
            );
            assert_eq!(expect["pass"], true, "both assertions hold: {expect}");

            // -- wait_until: live sim motion (discs advance 0.01 m of X per tick) ---
            let started = Instant::now();
            let waited = call(
                &mut client,
                "play.wait_until",
                json!({
                    "condition": {
                        "entity": { "component": "SimId" }, "component": "Transform",
                        "path": "translation.x", "op": "gte", "value": 2.5
                    },
                    "timeout_ms": 60_000
                }),
            );
            assert_eq!(waited["satisfied"], true, "the sim moved the disc: {waited}");
            assert!(started.elapsed() < SCRIPT_TIMEOUT);

            // -- Events: the startup sim event and its toast ---------------------
            let events = call(&mut client, "events.read", json!({}));
            let records = events["records"].as_array().expect("records");
            assert!(
                records
                    .iter()
                    .any(|r| r["kind"] == "sim_event" && r["data"]["kind"] == "sim_started"),
                "the sim_started event was tapped: {events}"
            );
            assert!(
                records
                    .iter()
                    .any(|r| r["kind"] == "toast" && r["data"]["text"] == "Sim online"),
                "the router's toast was tapped: {events}"
            );

            // -- Scene edits with provenance ------------------------------------
            let spawned = call(
                &mut client,
                "scene.spawn",
                json!({
                    "name": "Agent Probe",
                    "components": { "Transform": {
                        "translation": [0.0, 5.0, 0.0],
                        "rotation": [0.0, 0.0, 0.0, 1.0],
                        "scale": [1.0, 1.0, 1.0]
                    } }
                }),
            );
            let probe = spawned["entity"].as_u64().expect("spawned id");

            let preview = call(
                &mut client,
                "scene.set_property",
                json!({
                    "entity": probe, "component": "Transform", "path": "translation.y",
                    "value": 9.0, "dry_run": true
                }),
            );
            assert_eq!(preview["applied"], false, "dry_run does not apply: {preview}");
            let applied = call(
                &mut client,
                "scene.set_property",
                json!({
                    "entity": probe, "component": "Transform", "path": "translation.y", "value": 9.0
                }),
            );
            assert_eq!(applied["applied"], true);

            let changes = call(&mut client, "agent.changes", json!({}));
            let records = changes["records"].as_array().expect("change records");
            assert!(records.len() >= 2, "spawn + set_property are recorded: {changes}");
            let edit = records
                .iter()
                .find(|r| r["method"] == "scene.set_property")
                .expect("the edit is stamped");
            assert_eq!(
                edit["client"], "smoke-test",
                "provenance carries the client: {edit}"
            );
            assert_eq!(edit["model"], "glm");
            assert!(edit["target"].as_str().unwrap_or("").contains("translation.y"));

            // -- Live settings + the settings event tap --------------------------
            let set = call(
                &mut client,
                "settings.set",
                json!({ "path": "input.sensitivity", "value": 1.25 }),
            );
            assert_eq!(set["after"], 1.25, "{set}");
            let events = call(&mut client, "events.read", json!({ "since": 0, "limit": 500 }));
            assert!(
                events["records"]
                    .as_array()
                    .expect("records")
                    .iter()
                    .any(|r| r["kind"] == "settings"
                        && r["data"]["field"] == "input"
                        && r["data"]["after"]["input"]["sensitivity"] == 1.25),
                "the settings change was tapped: {events}"
            );

            // -- Step control ----------------------------------------------------
            let paused = call(&mut client, "play.pause", json!({}));
            assert_eq!(paused["paused"], true);
            let stepped = call(
                &mut client,
                "play.step",
                json!({ "frames": 3, "timeout_ms": 30_000 }),
            );
            assert!(stepped["frame"].as_u64().unwrap_or(0) > 0, "{stepped}");
            let resumed = call(&mut client, "play.resume", json!({}));
            assert_eq!(resumed["paused"], false);

            // -- Logs ------------------------------------------------------------
            let logs = call(&mut client, "logs.read", json!({ "limit": 10 }));
            if logs_installed {
                assert!(
                    logs["records"].as_array().is_some_and(|r| !r.is_empty()),
                    "the running app has log records: {logs}"
                );
            } else {
                assert!(
                    logs["next"].is_number(),
                    "logs.read answers even without the ring installed: {logs}"
                );
            }

            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    // Pump frames until the script finishes (or the guard fires). `update` does not run plugin
    // `finish` — only `run` does — and the agent server starts in `finish`, so call it here.
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = app(project.clone());
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
fn read_token(project: &std::path::Path) -> String {
    std::fs::read_to_string(endpoint::agent_dir(project).join("token")).expect("token file")
}
