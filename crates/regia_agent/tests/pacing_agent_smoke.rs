// SPDX-License-Identifier: MPL-2.0
//! End-to-end smoke for frame pacing (N12, ADR-1942, spec 23): a client drives `pacing.*` over the
//! real protocol while the frame loop runs a `DisplayPacer` on a stand-in vsync thread, the way the
//! runner does on a phone. The client sees the pacer take over, follows a forced interval, turns
//! pacing off and back on, and reads a simulated run. No window, no GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_platform::frame_pacing::{
    DisplayPacer, FramePacingReport, FramePacingSettings, FramePacingTrace, PacingSource, VsyncTicks,
};
use serde_json::{Value, json};

/// The stand-in panel's vsync period: 62.5 Hz.
const PERIOD_NS: u64 = 16_000_000;
/// `pacing.get` calls one expectation may take.
const MAX_POLLS: u32 = 5_000;

/// Calls one protocol method, unwrapping errors with the method name in the message.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

/// Polls `pacing.get` until `wanted` holds; the frame loop applies a change on its next frame.
fn wait_for_pacing(client: &mut Client, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..MAX_POLLS {
        let got = call(client, "pacing.get", json!({}));
        if wanted(&got) {
            return got;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("pacing never reached: {what}");
}

/// Reads the bearer token the app wrote.
fn read_token(agent_dir: &std::path::Path) -> String {
    std::fs::read_to_string(agent_dir.join("token")).expect("token file")
}

fn connect(agent_dir: &std::path::Path) -> Client {
    for _ in 0..2000 {
        if let Ok((advertised, _token)) = endpoint::read(agent_dir)
            && let Ok(connected) = Client::connect(&advertised.url, &read_token(agent_dir))
        {
            return connected;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no endpoint at {}", agent_dir.display());
}

fn conversation(client: &mut Client) {
    // Discovery: the methods and the guide topic are there.
    let schema = call(client, "agent.schema", json!({}));
    let names: Vec<&str> = schema["methods"]
        .as_array()
        .expect("methods")
        .iter()
        .map(|m| m["name"].as_str().expect("name"))
        .collect();
    for expected in ["pacing.get", "pacing.set", "pacing.simulate", "pacing.trace"] {
        assert!(names.contains(&expected), "schema lists {expected}");
    }
    let guide = call(client, "agent.guide", json!({ "topic": "frame-pacing" }));
    assert!(guide["text"].as_str().expect("text").contains("pacing.simulate"));
    assert!(guide["text"].as_str().expect("text").contains("pacing.trace"));

    // The pacer learns the panel from the ticks and takes the frames over.
    let paced = wait_for_pacing(client, "choreographer", |got| got["source"] == "choreographer");
    assert_eq!(paced["mode"], "auto", "{paced}");
    assert_eq!(paced["available"], true, "{paced}");
    assert_eq!(paced["swap_interval"], 1, "{paced}");
    let hz = paced["refresh_hz"].as_f64().expect("a rate");
    assert!((hz - 62.5).abs() < 0.1, "{paced}");

    // A forced interval reaches the frame loop.
    let set = call(client, "pacing.set", json!({ "mode": "interval", "interval": 2 }));
    assert_eq!(set["before"]["mode"], "auto", "{set}");
    assert_eq!(set["after"]["interval"], 2, "{set}");
    let forced = wait_for_pacing(client, "two vsyncs", |got| got["swap_interval"] == 2);
    assert_eq!(forced["slot_ms"], 32.0, "{forced}");

    // Off hands the frames to the software path, and the ticks keep counting.
    call(client, "pacing.set", json!({ "mode": "off" }));
    let off = wait_for_pacing(client, "software", |got| got["source"] == "software");
    assert_eq!(off["swap_interval"], Value::Null, "{off}");
    assert_eq!(off["available"], true, "{off}");

    call(client, "pacing.set", json!({ "mode": "auto" }));
    wait_for_pacing(client, "paced again", |got| got["source"] == "choreographer");

    // A refused call changes nothing.
    assert!(client.call("pacing.set", json!({ "mode": "fast" })).is_err());
    assert_eq!(call(client, "pacing.get", json!({}))["mode"], "auto");

    // The trace holds the frames the pacer released: 2 ms each, half of it in the swapchain.
    let mut traced = Value::Null;
    for _ in 0..MAX_POLLS {
        traced = call(client, "pacing.trace", json!({ "frames": 30 }));
        if traced["summary"]["paced"].as_u64().unwrap_or(0) >= 10 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(traced["summary"]["paced"].as_u64().unwrap_or(0) >= 10, "{traced}");
    let last = traced["rows"].as_array().and_then(|r| r.last()).expect("a row");
    assert_eq!(last["cost_ms"], 2.0, "{last}");
    assert_eq!(last["swapchain_wait_ms"], 1.0, "{last}");
    assert_eq!(last["work_ms"], 1.0, "{last}");
    assert!(client.call("pacing.trace", json!({ "frames": 0 })).is_err());
    // The cost knob reaches the settings and back.
    let cost = call(client, "pacing.set", json!({ "cost": "frame" }));
    assert_eq!(cost["after"]["cost"], "frame", "{cost}");
    call(client, "pacing.set", json!({ "cost": "work" }));

    // The model on virtual time, over the same protocol.
    let simulated = call(
        client,
        "pacing.simulate",
        json!({ "refresh_hz": 60, "frame_ms": 20, "frames": 300 }),
    );
    assert_eq!(simulated["swap_interval"], 2, "{simulated}");
    assert_eq!(simulated["tail_late"], 0, "{simulated}");
    assert!(
        client
            .call("pacing.simulate", json!({ "refresh_hz": 1000 }))
            .is_err()
    );
}

#[test]
fn pacing_agent_smoke() {
    let project = regia_test_utils::unique_temp_dir("agent-pacing");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(regia_agent::AgentPlugin)
        .init_resource::<FramePacingSettings>()
        .init_resource::<FramePacingReport>()
        .init_resource::<FramePacingTrace>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.finish_plugins();

    let ticks = VsyncTicks::new();
    let stop = Arc::new(AtomicBool::new(false));
    let vsync = {
        let (ticks, stop) = (ticks.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut time = PERIOD_NS;
            while !stop.load(Ordering::Relaxed) {
                ticks.tick(time);
                time += PERIOD_NS;
                std::thread::sleep(Duration::from_micros(200));
            }
        })
    };
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("agent-pacing-smoke".into())
        .spawn(move || conversation(&mut connect(&agent_dir)))
        .expect("client thread");

    let mut pacer = DisplayPacer::new(ticks, PacingSource::Choreographer, || 0);
    while !script.is_finished() {
        app.update();
        let settings = *app.world().resource::<FramePacingSettings>();
        let (cost, blocked) = (Duration::from_millis(2), Duration::from_millis(1));
        let waited = pacer.pace_frame(cost, blocked, None, settings);
        let row = pacer.trace_row(cost, blocked, waited);
        app.world_mut().resource_mut::<FramePacingTrace>().push(row);
        *app.world_mut().resource_mut::<FramePacingReport>() = pacer.report(settings.mode);
    }
    stop.store(true, Ordering::Relaxed);
    let joined = script.join();
    vsync.join().expect("vsync thread");
    let _ = std::fs::remove_dir_all(&project);
    joined.expect("the scripted conversation finished");
}
