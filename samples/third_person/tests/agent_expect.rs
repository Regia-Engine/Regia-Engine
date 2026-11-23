// SPDX-License-Identifier: MIT-0
//! Agent-protocol reachability for every M20 acceptance feature (`play.expect`).
//!
//! Headless: `MinimalPlugins` plus the sample, with a stand-in primary window so
//! `play.inject_input` can deliver the same key events the windowed sample reads.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};
use third_person::ThirdPersonSample;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60);

fn app(project: PathBuf) -> App {
    let mut app = App::new();
    // Straight into play (the title is covered by tests/combat.rs); saves and settings under the
    // temporary project.
    app.insert_resource(regia_framework::prelude::GameFlowSettings {
        skip_main_menu: true,
        ..Default::default()
    })
    .insert_resource(third_person::SampleDataDir(project.join(".data")));
    app.add_plugins((MinimalPlugins, ThirdPersonSample))
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project),
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.world_mut().spawn(regia_platform::PrimaryWindow);
    app
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn expect(client: &mut Client, name: &str, condition: Value) {
    let report = call(
        client,
        "play.expect",
        json!({ "assertions": [{ "name": name, "condition": condition }] }),
    );
    assert_eq!(report["pass"], true, "{name}: {report}");
}

fn wait_until(client: &mut Client, condition: Value) -> Value {
    let params = json!({ "condition": condition, "timeout_ms": 20_000 });
    match client.call("play.wait_until", params) {
        Ok(value) => value,
        Err(err) => panic!("play.wait_until {condition} failed: {err:?}"),
    }
}

/// Steps the paused game one frame at a time (`play.step`) and evaluates `condition` with
/// `play.expect` after each frame, until it holds or `max_frames` frames have run. Returns the
/// frame (1-based) it first held on, or the last report when it never did.
///
/// For a condition that holds only for a few frames (the fox on the flight, the jump's apex):
/// the caller pauses before the input that starts the motion, so every frame from that input on
/// is checked however slowly this client thread runs. A `wait_until` sent after the input only
/// sees the frames after it arrives, and the app ticks freely in between (a starved client under
/// load let the fox cross the flight first; it then walked to the far wall).
fn step_until(client: &mut Client, name: &str, condition: Value, max_frames: u32) -> Result<u32, Value> {
    let mut last = Value::Null;
    for frame in 1..=max_frames {
        call(client, "play.step", json!({ "frames": 1 }));
        last = call(
            client,
            "play.expect",
            json!({ "assertions": [{ "name": name, "condition": condition }] }),
        );
        if last["pass"] == true {
            return Ok(frame);
        }
    }
    Err(last)
}

fn key(client: &mut Client, key: &str, state: &str) {
    call(
        client,
        "play.inject_input",
        json!({ "events": [{ "type": "key", "key": key, "state": state }] }),
    );
}

/// First entity that has `component`, as the numeric id `scene.set_property` wants.
fn entity_with(client: &mut Client, component: &str) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "component": component, "limit": 4 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no entity with {component}: {listed}"))
}

fn set_f32(client: &mut Client, entity: u64, component: &str, path: &str, value: f32) {
    call(
        client,
        "scene.set_property",
        json!({ "entity": entity, "component": component, "path": path, "value": value }),
    );
}

fn collected_of(client: &mut Client, state: u64) -> u64 {
    let entity = call(
        client,
        "scene.get_entity",
        json!({ "entity": state, "components": ["SampleState"] }),
    );
    entity["components"]["SampleState"]["value"]["collected"]
        .as_u64()
        .unwrap_or_else(|| panic!("SampleState.collected missing: {entity}"))
}

/// Drops leftover jump velocity and the jump buffer, then puts the capsule on the ground.
///
/// A teleport that only writes x/z while the jump is still buffered launches the capsule
/// over the coin; the gameplay tests place it at rest height instead.
fn place_on_ground(client: &mut Client, character: u64, x: f32, y: f32, z: f32) {
    for axis in ["x", "y", "z"] {
        set_f32(
            client,
            character,
            "CharacterState",
            &format!("velocity.{axis}"),
            0.0,
        );
    }
    set_f32(client, character, "CharacterState", "jump_buffer_time_left", 0.0);
    set_f32(client, character, "CharacterState", "coyote_time_left", 0.0);
    set_f32(client, character, "Transform", "translation.x", x);
    set_f32(client, character, "Transform", "translation.y", y);
    set_f32(client, character, "Transform", "translation.z", z);
}

fn read_token(project: &std::path::Path) -> String {
    std::fs::read_to_string(endpoint::agent_dir(project).join("token")).expect("token file")
}

#[test]
fn agent_expect_covers_each_m20_feature() {
    // The foot-IK step below needs the Fox rig (`fox-rig-0`, `SolvedFoot`). Cook it here,
    // before the script's clock starts (exclusive across processes), instead of relying on
    // another test binary of this package having cooked the gitignored cache first: in a fresh
    // tree, or when the farm gate runs this binary before the lib / `gameplay` cook finished,
    // the sample loads capsule-only and the foot-IK wait can never hold.
    third_person::assets::ensure_cooked()
        .expect("Fox.glb (set REGIA_KHRONOS_ASSETS or allow the one-time download)");
    let project = std::env::temp_dir().join(format!("regia-m20-agent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("project dir");
    // `step_until` sends one mutating `play.step` per frame, faster than the default budget of
    // 50 mutating calls a second (specs/23 § 5); this project grants the script more.
    let agent_config = project.join(".regia");
    std::fs::create_dir_all(&agent_config).expect("agent config dir");
    std::fs::write(agent_config.join("agent.toml"), "rate_limit_per_second = 10000\n").expect("agent.toml");

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("m20-agent-client".into())
        .spawn(move || {
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

            call(
                &mut client,
                "agent.hello",
                json!({ "client": "m20-expect", "model": "worker" }),
            );

            let presented = wait_until(
                &mut client,
                json!({
                    "entity": { "component": "SampleState" },
                    "component": "SampleState",
                    "path": "total",
                    "op": "eq",
                    "value": 8
                }),
            );
            assert_eq!(presented["satisfied"], true, "level presented: {presented}");
            let grounded = wait_until(
                &mut client,
                json!({
                    "entity": { "component": "Character" },
                    "component": "CharacterState",
                    "path": "grounded",
                    "op": "eq",
                    "value": true
                }),
            );
            assert_eq!(grounded["satisfied"], true, "settled on the ground: {grounded}");

            // Move.
            key(&mut client, "KeyD", "pressed");
            let walked = wait_until(
                &mut client,
                json!({
                    "entity": { "component": "Character" },
                    "component": "Transform",
                    "path": "translation.x",
                    "op": "gte",
                    "value": -2.0
                }),
            );
            assert_eq!(walked["satisfied"], true, "walked forward: {walked}");
            key(&mut client, "KeyD", "released");

            // Jump: the apex is a few frames, so step them (see `step_until`).
            call(&mut client, "play.pause", json!({}));
            key(&mut client, "Space", "pressed");
            let jumped = step_until(
                &mut client,
                "jumped",
                json!({
                    "entity": { "component": "Character" },
                    "component": "Transform",
                    "path": "translation.y",
                    "op": "gte",
                    "value": 1.6
                }),
                120,
            );
            assert!(jumped.is_ok(), "jumped within 120 frames: {jumped:?}");
            key(&mut client, "Space", "released");
            call(&mut client, "play.resume", json!({}));

            let character = entity_with(&mut client, "Character");
            let state = entity_with(&mut client, "SampleState");

            // Collectible via the trigger script (place next to the first coin, then walk).
            place_on_ground(&mut client, character, -1.5, 1.05, -3.0);
            key(&mut client, "KeyD", "pressed");
            let collected = wait_until(
                &mut client,
                json!({
                    "entity": state,
                    "component": "SampleState",
                    "path": "collected",
                    "op": "gte",
                    "value": 1
                }),
            );
            assert_eq!(collected["satisfied"], true, "collected a coin: {collected}");
            key(&mut client, "KeyD", "released");
            let saved_count = collected_of(&mut client, state);
            expect(
                &mut client,
                "HUD counter string",
                json!({
                    "entity": { "name": "hud-counter" },
                    "component": "HudLabel",
                    "path": "text",
                    "op": "eq",
                    "value": format!("Coins: {saved_count} / 8")
                }),
            );

            // Save / load restores the collected count (collect one more, then load).
            let saved = call(&mut client, "save.save", json!({ "slot": "Chapter1" }));
            assert!(saved["bytes_written"].as_u64().is_some_and(|n| n > 0), "{saved}");
            place_on_ground(&mut client, character, 2.5, 1.05, -2.2);
            key(&mut client, "KeyD", "pressed");
            let more = wait_until(
                &mut client,
                json!({
                    "entity": state,
                    "component": "SampleState",
                    "path": "collected",
                    "op": "gte",
                    "value": saved_count + 1
                }),
            );
            assert_eq!(more["satisfied"], true, "collected another coin: {more}");
            key(&mut client, "KeyD", "released");
            // Step off the coin before loading. A respawned sensor the capsule is already
            // inside raises TriggerEnter on the next scan and would re-collect it.
            place_on_ground(&mut client, character, -4.0, 1.05, 2.0);
            let loaded = call(&mut client, "save.load", json!({ "slot": "Chapter1" }));
            assert!(
                loaded["resource_count"].as_u64().is_some_and(|n| n >= 1),
                "{loaded}"
            );
            let restored = wait_until(
                &mut client,
                json!({
                    "entity": state,
                    "component": "SampleState",
                    "path": "collected",
                    "op": "eq",
                    "value": saved_count
                }),
            );
            assert_eq!(restored["satisfied"], true, "save restored collected: {restored}");

            // Split-screen.
            key(&mut client, "F2", "pressed");
            let split = wait_until(
                &mut client,
                json!({
                    "entity": { "name": "player-camera-1" },
                    "component": "SplitViewport",
                    "path": "w",
                    "op": "eq",
                    "value": 0.5
                }),
            );
            assert_eq!(split["satisfied"], true, "second viewport: {split}");
            expect(
                &mut client,
                "player 0 keeps the left half",
                json!({
                    "entity": { "name": "player-camera-0" },
                    "component": "SplitViewport",
                    "path": "x",
                    "op": "eq",
                    "value": 0.0
                }),
            );
            let players = call(&mut client, "players.list", json!({}));
            assert_eq!(players["count"], 2, "{players}");

            // Foot IK on the stairs, before the boom teleport moves the fox off them.
            // One predicate, checked on every frame of the walk: the game is paused before
            // the teleport and the key, then stepped (see `step_until`), so the frames the
            // fox spends on the flight (about four with two feet planted on different
            // steps) cannot pass unchecked. `SolvedFoot.span` is the planted-foot ground
            // span from the solved joint tips; it stays ~0 if the rig's `FootPlacement` or the graph's
            // TwoBoneIK nodes are removed. 400 frames walk about 28 m at 4.5 m/s, past the
            // flight (x = 12.8 is about 17 m from the start).
            call(&mut client, "play.pause", json!({}));
            place_on_ground(&mut client, character, -4.0, 1.05, 0.0);
            key(&mut client, "KeyD", "pressed");
            let planted = step_until(
                &mut client,
                "foot joints span two steps while the capsule is on the flight",
                json!({
                    "all": [
                        {
                            "entity": character,
                            "component": "Transform",
                            "path": "translation.x",
                            "op": "gte",
                            "value": 10.6
                        },
                        {
                            "entity": character,
                            "component": "Transform",
                            "path": "translation.x",
                            "op": "lt",
                            "value": 12.8
                        },
                        {
                            "entity": character,
                            "component": "Transform",
                            "path": "translation.y",
                            "op": "gte",
                            "value": 1.55
                        },
                        {
                            "entity": { "name": "fox-rig-0" },
                            "component": "SolvedFoot",
                            "path": "span",
                            "op": "gte",
                            "value": 0.15
                        }
                    ]
                }),
                400,
            );
            assert!(
                planted.is_ok(),
                "foot joints span two steps while the capsule is on the flight: {planted:?}"
            );
            key(&mut client, "KeyD", "released");
            call(&mut client, "play.resume", json!({}));

            // Boom shortens against the +Z wall.
            set_f32(&mut client, character, "Transform", "translation.x", 0.0);
            set_f32(&mut client, character, "Transform", "translation.z", 27.0);
            let boom = wait_until(
                &mut client,
                json!({
                    "entity": { "name": "player-camera-0" },
                    "component": "CameraRuntime",
                    "path": "boom",
                    "op": "lt",
                    "value": 3.5
                }),
            );
            assert_eq!(boom["satisfied"], true, "boom shortened: {boom}");

            // Pause menu.
            key(&mut client, "Escape", "pressed");
            let paused = wait_until(
                &mut client,
                json!({
                    "entity": { "component": "SampleState" },
                    "component": "SampleState",
                    "path": "paused",
                    "op": "eq",
                    "value": true
                }),
            );
            assert_eq!(paused["satisfied"], true, "paused: {paused}");

            // Game states: Esc moved GameFlow to Paused; state.set back to Playing closes
            // the menu and unpauses (the sample's OnExit(GameFlow::Paused) hook).
            let flow_paused = wait_until(&mut client, json!({ "state": "GameFlow", "value": "Paused" }));
            assert_eq!(flow_paused["satisfied"], true, "GameFlow paused: {flow_paused}");
            let page = call(&mut client, "state.get", json!({ "state": "PausePage" }));
            assert_eq!(page["current"], "Main", "pause page exists while paused: {page}");
            let set = call(
                &mut client,
                "state.set",
                json!({ "state": "GameFlow", "value": "Playing" }),
            );
            assert_eq!(set["before"], "Paused", "{set}");
            let resumed = wait_until(
                &mut client,
                json!({ "all": [
                    { "state": "GameFlow", "value": "Playing" },
                    {
                        "entity": { "component": "SampleState" },
                        "component": "SampleState",
                        "path": "paused",
                        "op": "eq",
                        "value": false
                    },
                    { "resource": "Time", "path": "paused", "op": "eq", "value": false }
                ] }),
            );
            assert_eq!(
                resumed["satisfied"], true,
                "state.set resumed the game: {resumed}"
            );
            let states = call(&mut client, "state.list", json!({}));
            assert_eq!(states["count"], 3, "GameFlow, InGame, PausePage: {states}");

            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = app(project.clone());
    app.finish_plugins();
    while !script_ok.load(Ordering::Relaxed) && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "agent expect script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    let _ = std::fs::remove_dir_all(&project);
}

/// Rule 8 dogfood for the gameplay systems the sample gained in ADR-2134: gameplay tags, the car
/// (`physics.vehicles`, the `E` key, `physics.vehicle_input`) and its camera-driver chase camera
/// (`camera.inspect`).
#[test]
fn agent_drives_the_car_and_reads_the_tags() {
    let project = regia_test_utils::unique_temp_dir("regia-wc-third-person");
    let _ = std::fs::create_dir_all(&project);
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("wc-third-person-agent".into())
        .spawn(move || {
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
            call(
                &mut client,
                "agent.hello",
                json!({ "client": "wc-third-person", "model": "worker" }),
            );
            let character = entity_with(&mut client, "CharacterState");
            let car = entity_with(&mut client, "Car");

            // Tags: the table is loaded and the entities carry their tags.
            let listed = call(&mut client, "tags.list", json!({ "prefix": "State" }));
            let names: Vec<&str> = listed["tags"]
                .as_array()
                .map(|t| t.iter().filter_map(|row| row["name"].as_str()).collect())
                .unwrap_or_default();
            assert!(names.contains(&"State.Driving"), "{listed}");
            let player = call(
                &mut client,
                "tags.query",
                json!({ "entity": character, "query": { "Has": "Role.Player" } }),
            );
            assert_eq!(player["matches"], true, "{player}");
            let car_tags = call(&mut client, "tags.get", json!({ "entity": car }));
            assert_eq!(car_tags["tags"], json!(["Vehicle.Car"]), "{car_tags}");

            // The car rests on four wheels.
            let vehicles = call(&mut client, "physics.vehicles", json!({ "entity": car }));
            assert_eq!(vehicles["vehicles"][0]["wheel_count"], 4, "{vehicles}");

            // Walk up to it and press E: the character drives and the camera runs the asset.
            let start = call(
                &mut client,
                "scene.get_entity",
                json!({ "entity": car, "components": ["Transform"] }),
            );
            let at = &start["components"]["Transform"]["value"]["translation"];
            let (x, z) = (
                at[0].as_f64().unwrap_or(-12.0) as f32,
                at[2].as_f64().unwrap_or(-10.0) as f32,
            );
            // Paused from here: every frame is an explicit `play.step`.
            call(&mut client, "play.pause", json!({}));
            place_on_ground(&mut client, character, x + 2.4, 1.3, z);
            call(&mut client, "play.step", json!({ "frames": 5 }));
            let driving_now = |client: &mut Client| {
                let read = call(
                    client,
                    "scene.get_entity",
                    json!({ "entity": character, "components": ["Driving"] }),
                );
                read["components"]["Driving"]["value"]["car"].as_u64()
            };
            key(&mut client, "KeyE", "pressed");
            call(&mut client, "play.step", json!({ "frames": 2 }));
            key(&mut client, "KeyE", "released");
            call(&mut client, "play.step", json!({ "frames": 2 }));
            assert_eq!(driving_now(&mut client), Some(car), "E next to the car gets in");
            let tagged = call(
                &mut client,
                "tags.query",
                json!({ "entity": character, "query": { "Has": "State.Driving" } }),
            );
            assert_eq!(tagged["matches"], true, "{tagged}");
            let camera = entity_with(&mut client, "PlayerCamera");
            let inspect = call(&mut client, "camera.inspect", json!({ "entity": camera }));
            assert!(inspect.to_string().contains("vehicle_chase"), "{inspect}");

            // Drive through the protocol: full throttle for two seconds moves the car.
            call(
                &mut client,
                "physics.vehicle_input",
                json!({ "entity": car, "throttle": 1.0 }),
            );
            call(&mut client, "play.step", json!({ "frames": 120 }));
            let moved = call(&mut client, "physics.vehicles", json!({ "entity": car }));
            let speed = moved["vehicles"][0]["forward_speed"].as_f64().unwrap_or(0.0);
            assert!(speed > 1.0, "the car drives forward: {moved}");

            // E again: out of the car.
            key(&mut client, "KeyE", "pressed");
            call(&mut client, "play.step", json!({ "frames": 2 }));
            key(&mut client, "KeyE", "released");
            call(&mut client, "play.step", json!({ "frames": 2 }));
            assert_eq!(driving_now(&mut client), None, "E in the car gets out");
            call(&mut client, "play.resume", json!({}));
            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = app(project.clone());
    app.finish_plugins();
    while !script_ok.load(Ordering::Relaxed) && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the car script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    let _ = std::fs::remove_dir_all(&project);
}
