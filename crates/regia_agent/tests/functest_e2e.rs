// SPDX-License-Identifier: MPL-2.0
//! End to end over the real agent server (spec 41 § 8 test 12): the § 5 recipe through a WebSocket client.
//! A script test is placed with `functest.create` (writing its `.functest.ron` into the project), run,
//! polled with `functest.status` and read with `functest.results`; a deliberately failing step must
//! come back `Failed` with its name. The app runs on the main thread with manual frames; the wall-clock
//! guard only bounds a hang and is never asserted against.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::prelude::*;
use regia_framework::functional::FunctionalTestPlugin;
use serde_json::{Value, json};

const GUARD: Duration = Duration::from_secs(90);

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

#[test]
fn recipe_over_the_wire_reports_the_failing_step() {
    let project = regia_test_utils::unique_temp_dir("functest_e2e");
    std::fs::create_dir_all(&project).expect("project dir");
    // `functest.create` resolves the project through this variable, as the agent server does.
    let agent_dir = endpoint::agent_dir(&project);
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let script_project: PathBuf = project.clone();

    let client_thread = std::thread::Builder::new()
        .name("functest-e2e-client".into())
        .spawn(move || {
            let mut client = None;
            // The server thread publishes its endpoint after the app starts; poll with a bounded attempt
            // count (the existing agent smoke tests wait the same way).
            for _ in 0..4000 {
                if let Ok((advertised, token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut client = client.expect("connects to the agent");
            let script = "(format_version: 1, steps: [\
                (name: \"fine\", expect: []),\
                (name: \"never true\", wait_until: Some((condition: (resource: \"Time\", path: \"nope\", op: \"eq\", value: 1), within_frames: 3))),\
            ])";
            let created = call(
                &mut client,
                "functest.create",
                json!({ "name": "doomed", "body": "script:tests/doomed.functest.ron", "script": script, "tags": ["demo"] }),
            );
            assert!(created["entity"].is_u64());
            assert!(script_project.join("tests/doomed.functest.ron").is_file(), "the script was written");
            let listed = call(&mut client, "functest.list", json!({}));
            assert_eq!(listed["tests"][0]["body"], "script:tests/doomed.functest.ron");
            let started = call(&mut client, "functest.run", json!({ "tags": ["demo"] }));
            assert_eq!(started["started"], 1);
            let mut finished = false;
            for _ in 0..1_000_000 {
                if call(&mut client, "functest.status", json!({}))["finished"] == true {
                    finished = true;
                    break;
                }
                std::thread::yield_now();
            }
            assert!(finished, "the run finished");
            let results = call(&mut client, "functest.results", json!({}));
            let test = &results["tests"][0];
            assert_eq!(test["verdict"], "Failed", "{results}");
            assert!(test["message"].as_str().expect("message").contains("step `never true` failed"), "{results}");
            assert_eq!(test["steps"][1]["name"], "never true");
            assert_eq!(results["exit_code"], 1);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("spawns the client");

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .add_plugins((regia_agent::AgentPlugin, FunctionalTestPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.finish_plugins();
    let deadline = Instant::now() + GUARD;
    while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
        app.update();
    }
    client_thread
        .join()
        .expect("the client thread finished without a failed assertion");
    assert!(done.load(Ordering::Relaxed), "the conversation did not complete");
    let _ = std::fs::remove_dir_all(&project);
}
