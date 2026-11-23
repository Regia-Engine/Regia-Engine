// SPDX-License-Identifier: MIT-0
//! Rule 8 dogfooding (ADR-2039): an agent drives a net_game client over the real agent protocol.
//! The client app runs the agent server; a script thread reads the link's endpoint report through
//! `net.status` and `net.crypto`, reads `net.prediction`, and turns the conditioner on with
//! `net.conditioner_set`, which the link applies on its next frame. A server and the client run
//! over real UDP with the manual link clock; no window, no GPU. `play.wait_until` and
//! `play.expect` judge the replicated avatar before the agent sets a seeded conditioner spec.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use net_game::{Side, headless_app, link_options};
use regia::agent::client::Client;
use regia::agent::endpoint;
use regia::net::ConditionerConfig;
use regia::net::link::{LinkClock, LinkOptions, NetLink};
use regia::prelude::*;
use serde_json::{Value, json};

/// Hang guard only: the conversation takes a few hundred frames.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

/// The endpoint labelled `client` in a `net.status` or `net.crypto` answer.
fn client_endpoint(answer: &Value) -> Option<Value> {
    answer["endpoints"]
        .as_array()?
        .iter()
        .find(|e| e["label"] == "client")
        .cloned()
}

#[test]
fn an_agent_reads_and_steers_the_link() {
    let project = regia_test_utils::temp::unique_temp_dir("net-game-agent");
    let clock = LinkClock::Manual {
        start_ms: 0,
        step_us: 16_667,
    };
    let mut server = headless_app(Side::Server);
    let opts = link_options(clock, ConditionerConfig::default());
    net_game::host_udp(&mut server, SocketAddr::from(([127, 0, 0, 1], 0)), &opts).expect("host");
    let server_addr = server.world().resource::<NetLink>().local_addr().expect("bound");
    let key = server.world().resource::<NetLink>().server_key();

    let mut client = headless_app(Side::Client);
    client
        .add_plugins(MinimalPlugins)
        .insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins(regia::agent::AgentPlugin);
    let client_opts = LinkOptions {
        server_key: key,
        ..opts
    };
    net_game::connect_udp(&mut client, server_addr, &client_opts).expect("connect");

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("net-game-agent".into())
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
            // The link's report: connected and sealed.
            let mut status = Value::Null;
            for _ in 0..500 {
                status = call(&mut agent, "net.status", json!({}));
                if client_endpoint(&status).is_some_and(|e| e["state"] == "connected") {
                    break;
                }
            }
            let report = client_endpoint(&status).expect("the link publishes under `client`");
            assert_eq!(report["state"], "connected", "{status}");
            assert_eq!(report["crypto"]["sealed"], true, "{status}");
            let crypto = call(&mut agent, "net.crypto", json!({}));
            assert!(client_endpoint(&crypto).is_some(), "{crypto}");
            let prediction = call(&mut agent, "net.prediction", json!({}));
            assert!(prediction.is_object(), "{prediction}");
            let condition = json!({
                "entity": { "component": "Avatar" }, "component": "Transform",
                "path": "translation.x", "op": "eq", "value": 6.0
            });
            let waited = call(&mut agent, "play.wait_until", json!({
                "condition": condition, "timeout_ms": 30_000
            }));
            assert_eq!(waited["satisfied"], true, "{waited}");
            let expected = call(&mut agent, "play.expect", json!({
                "assertions": [{ "name": "replicated avatar at spawn", "condition": condition }]
            }));
            assert_eq!(expected["pass"], true, "{expected}");
            // Steer: the conditioner reaches the link on its next frame.
            let set = call(
                &mut agent,
                "net.conditioner_set",
                json!({ "endpoint": "client", "spec": "delay=40,jitter=10,loss=2%,reorder=5%,reorder_max=20,seed=11" }),
            );
            assert_eq!(set["queued_for"], json!(["client"]), "{set}");
            assert!(set["conditions"].as_str().is_some_and(|s| s.ends_with("seed=11")), "{set}");
            let mut on = false;
            for _ in 0..500 {
                let status = call(&mut agent, "net.status", json!({}));
                if client_endpoint(&status).is_some_and(|e| {
                    e["conditioner"]["enabled"] == true && e["conditioner"]["seed"] == 11
                }) {
                    on = true;
                    break;
                }
            }
            assert!(on, "the link applied the agent's conditioner");
            flag.store(true, Ordering::Relaxed);
        })
        .expect("script thread");

    client.finish_plugins();
    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    while !done.load(Ordering::Relaxed) && !script.is_finished() && Instant::now() < deadline {
        server.update();
        client.update();
    }
    script.join().expect("the script passed");
    assert!(done.load(Ordering::Relaxed), "the conversation did not finish");
    let _ = std::fs::remove_dir_all(&project);
}
