// SPDX-License-Identifier: MIT-0
//! Agent dogfood for the systems the third-person sample shows beyond combat: the guide NPC's
//! conversation (lines, one choice, subtitles, two locales), the player's cloth cape and the soft
//! ball, and demo recording with scrubbing. Every check goes through the agent protocol on a
//! headless app (`MinimalPlugins` plus the sample, one fixed step per frame), with fixed frame
//! counts and no wall-clock budgets.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};
use third_person::{SampleDataDir, ThirdPersonSample};

/// Most app frames one phase may run (a hang guard counted in frames, not wall time).
const MAX_APP_FRAMES: u64 = 2_000_000;

fn app(project: &Path, data: &Path) -> App {
    let mut app = App::new();
    app.insert_resource(SampleDataDir(data.to_path_buf()));
    app.insert_resource(regia_framework::prelude::GameFlowSettings {
        skip_main_menu: true,
        ..Default::default()
    });
    app.add_plugins((MinimalPlugins, ThirdPersonSample))
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
        });
    // One fixed step per frame at 60 Hz: a demo seek lands on exactly the state a straight run reaches.
    enable_fixed_step(&mut app, 60.0);
    app.world_mut().spawn(regia_platform::PrimaryWindow);
    app
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn holds(client: &mut Client, condition: &Value) -> bool {
    let report = call(
        client,
        "play.expect",
        json!({ "assertions": [{ "name": "check", "condition": condition }] }),
    );
    report["pass"] == true
}

/// Steps `chunk` frames at a time until `condition` holds; the frames stepped, or `Err` at `max_frames`.
fn step_until(client: &mut Client, condition: Value, max_frames: u32, chunk: u32) -> Result<u32, u32> {
    let mut frames = 0;
    while frames < max_frames {
        call(client, "play.step", json!({ "frames": chunk }));
        frames += chunk;
        if holds(client, &condition) {
            return Ok(frames);
        }
    }
    Err(frames)
}

fn key(client: &mut Client, key: &str, state: &str) {
    call(
        client,
        "play.inject_input",
        json!({ "events": [{ "type": "key", "key": key, "state": state }] }),
    );
}

/// Presses and releases a key over two frames.
fn tap(client: &mut Client, name: &str) {
    key(client, name, "pressed");
    call(client, "play.step", json!({ "frames": 1 }));
    key(client, name, "released");
    call(client, "play.step", json!({ "frames": 1 }));
}

fn entity_named(client: &mut Client, name: &str) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "name": name, "limit": 2 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no entity named {name}: {listed}"))
}

fn entity_with(client: &mut Client, component: &str) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "component": component, "limit": 2 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no entity with {component}: {listed}"))
}

fn component(client: &mut Client, entity: u64, name: &str) -> Value {
    let read = call(
        client,
        "scene.get_entity",
        json!({ "entity": entity, "components": [name] }),
    );
    read["components"][name]["value"].clone()
}

fn connect(project: &Path) -> Client {
    let dir = endpoint::agent_dir(project);
    for _ in 0..3000 {
        if let Ok((advertised, _)) = endpoint::read(&dir)
            && let Ok(token) = std::fs::read_to_string(dir.join("token"))
            && let Ok(client) = Client::connect(&advertised.url, &token)
        {
            return client;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("no agent endpoint at {}", dir.display());
}

/// Runs `script` against a fresh app (already in `Playing`) until it finishes. The script gets
/// the agent client and the run's temp root (e.g. to assert files under `data/`).
fn run_phase(tag: &str, script: impl FnOnce(&mut Client, &Path) + Send + 'static) {
    let root: PathBuf = regia_test_utils::unique_temp_dir(tag);
    let project = root.join("project");
    let data = root.join("data");
    std::fs::create_dir_all(project.join(".regia")).expect("project dir");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(
        project.join(".regia/agent.toml"),
        "rate_limit_per_second = 100000\n",
    )
    .expect("agent.toml");
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let script_project = project.clone();
    let script_root = root.clone();
    let thread = std::thread::spawn(move || {
        let mut client = connect(&script_project);
        call(
            &mut client,
            "agent.hello",
            json!({ "client": tag_name(), "model": "worker" }),
        );
        call(&mut client, "play.pause", json!({}));
        let playing = step_until(
            &mut client,
            json!({ "state": "GameFlow", "op": "eq", "value": "Playing" }),
            30,
            1,
        );
        assert!(playing.is_ok(), "the flow reached Playing: {playing:?}");
        call(&mut client, "play.step", json!({ "frames": 10 }));
        script(&mut client, &script_root);
        flag.store(true, Ordering::Relaxed);
    });
    let mut app = app(&project, &data);
    app.finish_plugins();
    let mut frames = 0u64;
    while !done.load(Ordering::Relaxed) && !thread.is_finished() && frames < MAX_APP_FRAMES {
        app.update();
        frames += 1;
    }
    let joined = thread.join();
    assert!(joined.is_ok(), "the phase script panicked (see above)");
    assert!(
        done.load(Ordering::Relaxed),
        "the phase did not finish in {MAX_APP_FRAMES} frames"
    );
    drop(app);
}

fn tag_name() -> &'static str {
    "tp-systems"
}

fn talk(client: &mut Client) -> Value {
    let guide = entity_named(client, "npc-guide");
    component(client, guide, "TalkState")
}

fn lines(talk: &Value) -> Vec<String> {
    talk["lines_seen"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_guide_talks_offers_a_choice_and_speaks_two_locales() {
    run_phase("tp-sys-talk", |client, _root| {
        // Out of reach first: the talk key does nothing.
        let character = entity_with(client, "Character");
        for (axis, v) in [("x", 20.0), ("y", 1.1), ("z", 20.0)] {
            call(
                client,
                "scene.set_property",
                json!({ "entity": character, "component": "Transform", "path": format!("translation.{axis}"), "value": v }),
            );
        }
        call(client, "play.step", json!({ "frames": 3 }));
        tap(client, "KeyT");
        assert_eq!(talk(client)["talking"], false, "nobody in reach");
        // Back next to the guide.
        for (axis, v) in [("x", -5.0), ("y", 1.1), ("z", -1.0)] {
            call(
                client,
                "scene.set_property",
                json!({ "entity": character, "component": "Transform", "path": format!("translation.{axis}"), "value": v }),
            );
        }
        call(client, "play.step", json!({ "frames": 3 }));

        // English: the greeting, the question, then the choice waits.
        tap(client, "KeyT");
        call(client, "play.step", json!({ "frames": 10 }));
        let state = talk(client);
        assert_eq!(state["talking"], true, "{state}");
        assert_eq!(state["locale"], "en");
        let shown = call(client, "captions.state", json!({ "include_layout": false })).to_string();
        assert!(
            shown.contains("Mind the stairs"),
            "the subtitle shows the English text: {shown}"
        );
        assert!(
            shown.contains("guide#greet"),
            "the caption names its line: {shown}"
        );
        let asked = step_until(
            client,
            json!({ "entity": { "name": "npc-guide" }, "component": "TalkState", "path": "choice_pending", "op": "eq", "value": true }),
            400,
            5,
        );
        assert!(asked.is_ok(), "the choice was offered: {asked:?}");
        let state = talk(client);
        assert_eq!(lines(&state), ["guide#greet", "guide#ask"]);
        assert_eq!(state["options"], json!(["Gladly.", "Not now."]));
        // A second talk key mid-choice does not restart the run.
        tap(client, "KeyT");
        assert_eq!(talk(client)["run"], state["run"]);

        tap(client, "Digit1");
        let done = step_until(
            client,
            json!({ "entity": { "name": "npc-guide" }, "component": "TalkState", "path": "finished", "op": "gte", "value": 1 }),
            400,
            5,
        );
        assert!(done.is_ok(), "the first run ended: {done:?}");
        let state = talk(client);
        assert_eq!(lines(&state), ["guide#greet", "guide#ask", "guide#yes"]);
        assert_eq!(state["answer"], 0);
        assert_eq!(state["last_event"], "quest_given");
        assert_eq!(state["talking"], false);

        // German: the same conversation, the other answer.
        tap(client, "KeyL");
        tap(client, "KeyT");
        call(client, "play.step", json!({ "frames": 10 }));
        let state = talk(client);
        assert_eq!(state["locale"], "de");
        let shown = call(client, "captions.state", json!({ "include_layout": false })).to_string();
        assert!(
            shown.contains("Achte auf die Treppe"),
            "the subtitle shows the German text: {shown}"
        );
        let asked = step_until(
            client,
            json!({ "entity": { "name": "npc-guide" }, "component": "TalkState", "path": "choice_pending", "op": "eq", "value": true }),
            400,
            5,
        );
        assert!(asked.is_ok(), "the choice was offered again: {asked:?}");
        assert_eq!(talk(client)["options"], json!(["Sehr gern.", "Jetzt nicht."]));
        tap(client, "Digit2");
        let done = step_until(
            client,
            json!({ "entity": { "name": "npc-guide" }, "component": "TalkState", "path": "finished", "op": "gte", "value": 2 }),
            400,
            5,
        );
        assert!(done.is_ok(), "the second run ended: {done:?}");
        let state = talk(client);
        assert_eq!(lines(&state), ["guide#greet", "guide#ask", "guide#no"]);
        assert_eq!(state["answer"], 1);
    });
}

/// `physics.cloth_inspect` for the entity named `name`: its one solver.
fn solver(client: &mut Client, name: &str) -> Value {
    let entity = entity_named(client, name);
    let report = call(
        client,
        "physics.cloth_inspect",
        json!({ "target": { "entity": entity } }),
    );
    report["solvers"][0].clone()
}

/// `physics.cloth_inspect` for the entity named `name`, including every particle's position,
/// velocity and inverse mass (one slice covers the sample's solvers).
fn solver_particles(client: &mut Client, name: &str) -> Value {
    let entity = entity_named(client, name);
    let report = call(
        client,
        "physics.cloth_inspect",
        json!({ "target": { "entity": entity }, "particles": { "from": 0, "count": 256 } }),
    );
    report["solvers"][0]["particles"].clone()
}

fn bounds(solver: &Value) -> [f64; 6] {
    let mut out = [0.0; 6];
    for (i, key) in ["bounds_min", "bounds_max"].iter().enumerate() {
        for axis in 0..3 {
            out[i * 3 + axis] = solver[*key][axis].as_f64().unwrap_or(f64::NAN);
        }
    }
    out
}

fn drift(a: &[f64; 6], b: &[f64; 6]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max)
}

#[test]
fn the_cape_hangs_swings_in_wind_and_settles_and_the_ball_rests() {
    run_phase("tp-sys-cloth", |client, _root| {
        call(client, "play.step", json!({ "frames": 240 }));
        let cape = solver(client, "cape-0");
        assert_eq!(cape["kind"], "cloth", "{cape}");
        assert_eq!(cape["particles"], 66);
        assert_eq!(cape["pinned"], 11, "the top row is pinned to the mount joint");
        assert_eq!(cape["colliders"], 1, "the torso proxy");
        assert!(
            cape["steps"].as_u64().unwrap_or(0) > 100,
            "the solver ran: {cape}"
        );
        assert_eq!(cape["dropped_steps"], 0);

        // Resting: the bounds hold still over the next half second.
        let before = bounds(&cape);
        call(client, "play.step", json!({ "frames": 30 }));
        let rest = bounds(&solver(client, "cape-0"));
        assert!(drift(&before, &rest) < 0.01, "at rest: {before:?} vs {rest:?}");

        // Wind from behind the character blows it forward; it moves.
        let target = json!({ "entity": entity_named(client, "cape-0") });
        call(
            client,
            "physics.cloth_wind",
            json!({ "target": target, "velocity": [0.0, 0.0, -12.0] }),
        );
        call(client, "play.step", json!({ "frames": 40 }));
        let blown = bounds(&solver(client, "cape-0"));
        assert!(
            drift(&rest, &blown) > 0.02,
            "the wind moved the cape: {rest:?} vs {blown:?}"
        );

        // Wind off: it settles again.
        call(
            client,
            "physics.cloth_wind",
            json!({ "target": target, "velocity": [0.0, 0.0, 0.0], "clear": true }),
        );
        call(client, "play.step", json!({ "frames": 300 }));
        let a = bounds(&solver(client, "cape-0"));
        call(client, "play.step", json!({ "frames": 30 }));
        let b = bounds(&solver(client, "cape-0"));
        assert!(drift(&a, &b) < 0.01, "settled again: {a:?} vs {b:?}");
        let settled = solver(client, "cape-0");
        assert!(settled["max_stretch"].as_f64().unwrap_or(9.0) < 1.15, "{settled}");

        // The solver is deterministic: reset and step the same count twice, same particle bounds.
        call(client, "physics.cloth_reset", json!({ "target": target }));
        call(
            client,
            "physics.cloth_step",
            json!({ "target": target, "count": 90 }),
        );
        let first = solver(client, "cape-0");
        let first_bounds = bounds(&first);
        call(client, "physics.cloth_reset", json!({ "target": target }));
        call(
            client,
            "physics.cloth_step",
            json!({ "target": target, "count": 90 }),
        );
        let second = solver(client, "cape-0");
        let second_bounds = bounds(&second);
        assert!(
            drift(&first_bounds, &second_bounds) < 1e-6,
            "same steps, same shape: {first_bounds:?} vs {second_bounds:?}"
        );
        // Bounds alone can hide interior divergence: compare every particle's position, velocity
        // and inverse mass. (The state digest is no help here — it folds in the lifetime step
        // count, which a reset keeps counting.)
        assert_eq!(
            solver_particles(client, "cape-0"),
            {
                call(client, "physics.cloth_reset", json!({ "target": target }));
                call(
                    client,
                    "physics.cloth_step",
                    json!({ "target": target, "count": 90 }),
                );
                solver_particles(client, "cape-0")
            },
            "same steps, same full particle state"
        );

        // The soft ball landed on the ground and holds its volume.
        let ball = solver(client, "soft-ball");
        assert_eq!(ball["kind"], "soft", "{ball}");
        let volume = ball["volume"].as_f64().unwrap_or(0.0);
        let rest_volume = ball["rest_volume"].as_f64().unwrap_or(1.0);
        assert!((volume / rest_volume - 1.0).abs() < 0.15, "volume kept: {ball}");
        let floor = bounds(&ball)[1];
        assert!(
            floor > 0.2 && floor < 0.6,
            "the ball rests near the ground (min y {floor})"
        );
        assert!(
            ball["contacts"].as_u64().unwrap_or(0) > 0,
            "touching the ground: {ball}"
        );
    });
}

#[test]
fn a_recorded_walk_scrubs_back_to_the_recorded_positions() {
    run_phase("tp-sys-demo", |client, root| {
        let character = entity_with(client, "Character");
        let position = |client: &mut Client| -> [f64; 3] {
            let t = component(client, character, "Transform");
            [
                t["translation"][0].as_f64().unwrap_or(f64::NAN),
                t["translation"][1].as_f64().unwrap_or(f64::NAN),
                t["translation"][2].as_f64().unwrap_or(f64::NAN),
            ]
        };
        // Stand still first so the recording starts from rest on the ground.
        call(client, "play.step", json!({ "frames": 60 }));
        key(client, "KeyW", "pressed");
        call(client, "play.step", json!({ "frames": 2 }));

        // The request applies at the end of its frame; the app is paused, so no step runs and the
        // next call (answered in the following frame) sees tick 0.
        call(
            client,
            "demo.record_start",
            json!({ "keyframe_interval": 20, "seed": 7 }),
        );
        // The driver applies the request in `Last` of the next updated frame; run an empty frame.
        call(client, "play.step", json!({ "frames": 1 }));
        let status = call(client, "demo.status", json!({}));
        assert_eq!(status["mode"], "recording", "{status}");
        // Recorded index i is the state at tick `base + i`.
        let base = status["tick"].as_u64().unwrap_or(u64::MAX);
        assert!(base <= 1, "recording starts at the first fixed steps: {status}");

        // Walk 120 recorded steps, noting the position after each.
        let mut recorded = vec![position(client)];
        for _ in 0..120 {
            call(client, "play.step", json!({ "frames": 1 }));
            recorded.push(position(client));
        }
        let status = call(client, "demo.status", json!({}));
        let steps = status["steps"].as_u64().unwrap_or(0);
        assert!(steps >= 120, "recorded {steps} steps: {status}");
        assert!(
            recorded[120][0] != recorded[0][0] || recorded[120][2] != recorded[0][2],
            "the walk moved the player: {:?} -> {:?}",
            recorded[0],
            recorded[120]
        );
        // Stop with F6 (not the agent): the key path saves under `<data dir>/demos/run.frec`,
        // creating the directory on a fresh data dir. The book promises the file (third-person-demo
        // page); assert it really landed on disk, not just in the driver's memory.
        tap(client, "F6");
        call(client, "play.step", json!({ "frames": 2 }));
        let status = call(client, "demo.status", json!({}));
        assert_eq!(status["mode"], "idle", "{status}");
        let saved = root.join("data").join("demos").join("run.frec");
        assert!(saved.is_file(), "F6 stopped into {}: {status}", saved.display());
        assert!(
            std::fs::metadata(&saved).map_or(0, |m| m.len()) > 0,
            "the recording was written: {}",
            saved.display()
        );
        assert!(
            status["keyframes"].as_array().map_or(0, Vec::len) >= 6,
            "{status}"
        );
        key(client, "KeyW", "released");

        // Play the recording, then scrub: forward, back, to the start and to the end.
        call(client, "demo.play", json!({}));
        call(client, "demo.pause", json!({ "paused": true }));
        call(client, "demo.status", json!({}));
        for k in [75u64, 30, 100, 0, 120, 45] {
            let tick = base + k;
            call(client, "demo.seek", json!({ "tick": tick }));
            let status = call(client, "demo.status", json!({}));
            assert!(status["last_error"].is_null(), "tick {tick}: {status}");
            assert_eq!(status["last_seek"]["tick"], tick, "{status}");
            let at = position(client);
            let want = recorded[k as usize];
            for axis in 0..3 {
                assert!(
                    (at[axis] - want[axis]).abs() < 1e-3,
                    "tick {tick} axis {axis}: scrubbed {at:?} recorded {want:?}"
                );
            }
        }
        // The keyframe hashes held on every seek: nothing diverged.
        let status = call(client, "demo.status", json!({}));
        assert!(status["diverged_at"].is_null(), "{status}");
    });
}
