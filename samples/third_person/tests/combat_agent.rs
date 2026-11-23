// SPDX-License-Identifier: MIT-0
//! Agent dogfood for the third-person demo's combat (ADR-2596): every check goes through the agent
//! protocol (`play.pause` / `play.step` fixed frames, `play.inject_input` scripted keys,
//! `scene.set_property` placement, `play.expect` / `state.*` / `settings.*` / `save.*`), on a
//! headless app (`MinimalPlugins` plus the sample, a stand-in primary window for input).
//!
//! Phase 1 (title screen on): settings through `settings.set`, play through `state.set`; a grunt
//! notices the player and reaches it; scripted `KeyF` presses kill it and a ragdoll appears; the
//! player activates checkpoint 1, dies to the second grunt and respawns there; `save.save`.
//! Phase 2 (a new app on the same data directory = quit and relaunch): the settings are back,
//! `save.load` restores the checkpoint and the defeated grunt.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
    app.add_plugins((MinimalPlugins, ThirdPersonSample))
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.to_path_buf()),
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

fn holds(client: &mut Client, condition: &Value) -> (bool, Value) {
    let report = call(
        client,
        "play.expect",
        json!({ "assertions": [{ "name": "check", "condition": condition }] }),
    );
    (report["pass"] == true, report)
}

fn expect(client: &mut Client, name: &str, condition: Value) {
    let (ok, report) = holds(client, &condition);
    assert!(ok, "{name}: {report}");
}

/// Steps `chunk` frames at a time until `condition` holds or `max_frames` ran; the frame count it
/// held after, or the last report.
fn step_until(client: &mut Client, condition: Value, max_frames: u32, chunk: u32) -> Result<u32, Value> {
    let mut last = Value::Null;
    let mut frames = 0;
    while frames < max_frames {
        call(client, "play.step", json!({ "frames": chunk }));
        frames += chunk;
        let (ok, report) = holds(client, &condition);
        if ok {
            return Ok(frames);
        }
        last = report;
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

fn set(client: &mut Client, entity: u64, component: &str, path: &str, value: f64) {
    call(
        client,
        "scene.set_property",
        json!({ "entity": entity, "component": component, "path": path, "value": value }),
    );
}

/// Puts the player capsule at rest at (x, y, z).
fn place(client: &mut Client, character: u64, x: f64, y: f64, z: f64) {
    for axis in ["x", "y", "z"] {
        set(
            client,
            character,
            "CharacterState",
            &format!("velocity.{axis}"),
            0.0,
        );
    }
    set(client, character, "Transform", "translation.x", x);
    set(client, character, "Transform", "translation.y", y);
    set(client, character, "Transform", "translation.z", z);
}

/// Places the player `meters` in front of enemy `name` (its facing from `Enemy.yaw`).
fn face_enemy(client: &mut Client, character: u64, name: &str, meters: f64) {
    let enemy = entity_named(client, name);
    let at = component(client, enemy, "Transform")["translation"].clone();
    let yaw = component(client, enemy, "Enemy")["yaw"].as_f64().unwrap_or(0.0);
    let (x, z) = (at[0].as_f64().unwrap_or(0.0), at[2].as_f64().unwrap_or(0.0));
    place(
        client,
        character,
        x - yaw.sin() * meters,
        1.05,
        z - yaw.cos() * meters,
    );
}

fn summary(path: &str, op: &str, value: Value) -> Value {
    json!({ "entity": { "component": "CombatSummary" }, "component": "CombatSummary",
            "path": path, "op": op, "value": value })
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
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no agent endpoint at {}", dir.display());
}

/// Runs `script` against a fresh app on (`project`, `data`) until it finishes.
fn run_phase(project: PathBuf, data: PathBuf, script: impl FnOnce(&mut Client) + Send + 'static) {
    std::fs::create_dir_all(project.join(".regia")).expect("project dir");
    std::fs::write(
        project.join(".regia/agent.toml"),
        "rate_limit_per_second = 100000\n",
    )
    .expect("agent.toml");
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let script_project = project.clone();
    let thread = std::thread::spawn(move || {
        let mut client = connect(&script_project);
        call(
            &mut client,
            "agent.hello",
            json!({ "client": "tp-combat", "model": "worker" }),
        );
        script(&mut client);
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
    // Dropping the app is the quit: pending settings are written.
    drop(app);
}

#[test]
fn agent_drives_combat_death_respawn_and_persistence() {
    let root = regia_test_utils::unique_temp_dir("tp-combat-agent");
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("data dir");

    run_phase(root.join("p1"), data.clone(), |client| {
        call(client, "play.pause", json!({}));
        call(client, "play.step", json!({ "frames": 5 }));
        expect(
            client,
            "title screen",
            json!({ "state": "GameFlow", "op": "eq", "value": "MainMenu" }),
        );

        // Settings through the engine's settings methods.
        call(
            client,
            "settings.set",
            json!({ "path": "audio.master", "value": 0.5 }),
        );
        call(
            client,
            "settings.set",
            json!({ "path": "input.invert_y", "value": true }),
        );
        call(
            client,
            "settings.set",
            json!({ "path": "input.sensitivity", "value": 1.5 }),
        );
        call(client, "play.step", json!({ "frames": 2 }));
        expect(
            client,
            "master volume",
            json!({ "resource": "GameSettings", "path": "audio.master", "op": "eq", "value": 0.5 }),
        );

        // Play.
        call(
            client,
            "state.set",
            json!({ "state": "GameFlow", "value": "Loading" }),
        );
        let playing = step_until(
            client,
            json!({ "state": "GameFlow", "op": "eq", "value": "Playing" }),
            10,
            1,
        );
        assert!(playing.is_ok(), "playing: {playing:?}");
        call(client, "play.step", json!({ "frames": 30 }));
        let character = entity_with(client, "Character");

        // 1. The grunt notices the player and reaches it.
        face_enemy(client, character, "enemy-0", 6.0);
        let noticed = step_until(
            client,
            json!({ "entity": { "name": "enemy-0" }, "component": "EnemyBrain", "path": "alert", "op": "eq", "value": true }),
            120,
            2,
        );
        assert!(noticed.is_ok(), "noticed: {noticed:?}");
        let reached = step_until(
            client,
            json!({ "a": { "name": "enemy-0" }, "b": { "component": "Character" }, "op": "within", "meters": 2.0 }),
            600,
            5,
        );
        assert!(reached.is_ok(), "reached: {reached:?}");

        // 2. Scripted attacks kill it; a ragdoll appears.
        let mut killed = false;
        for _ in 0..90 {
            key(client, "KeyF", "pressed");
            call(client, "play.step", json!({ "frames": 2 }));
            key(client, "KeyF", "released");
            call(client, "play.step", json!({ "frames": 8 }));
            if holds(client, &summary("enemies_killed", "gte", json!(1))).0 {
                killed = true;
                break;
            }
        }
        assert!(killed, "scripted attacks killed the grunt");
        let corpse = step_until(
            client,
            json!({ "entity": { "component": "Corpse" }, "component": "Corpse", "path": "bodies", "op": "gte", "value": 8 }),
            30,
            1,
        );
        assert!(corpse.is_ok(), "a ragdoll with its bodies: {corpse:?}");
        expect(client, "defeated list", summary("defeated", "eq", json!([0])));

        // 3. Checkpoint 1, then death to the second grunt and the respawn there.
        let pad = entity_named(client, "checkpoint-1");
        let at = component(client, pad, "Transform")["translation"].clone();
        place(
            client,
            character,
            at[0].as_f64().unwrap_or(0.0),
            1.1,
            at[2].as_f64().unwrap_or(0.0),
        );
        let reached_pad = step_until(client, summary("checkpoint", "eq", json!(1)), 30, 1);
        assert!(reached_pad.is_ok(), "checkpoint 1: {reached_pad:?}");
        face_enemy(client, character, "enemy-2", 2.0);
        let died = step_until(client, summary("player_deaths", "gte", json!(1)), 2400, 10);
        assert!(died.is_ok(), "the player died: {died:?}");
        let respawned = step_until(client, summary("respawns", "gte", json!(1)), 300, 5);
        assert!(respawned.is_ok(), "respawned: {respawned:?}");
        call(client, "play.step", json!({ "frames": 3 }));
        expect(
            client,
            "at checkpoint 1",
            json!({ "a": { "component": "Character" }, "b": { "name": "checkpoint-1" }, "op": "within", "meters": 1.8 }),
        );
        expect(
            client,
            "full health",
            summary("player_health", "eq", json!(100.0)),
        );

        // 4. Save.
        let saved = call(client, "save.save", json!({ "slot": "Chapter1" }));
        assert!(saved["bytes_written"].as_u64().is_some_and(|n| n > 0), "{saved}");
    });

    // Quit and relaunch on the same data directory.
    run_phase(root.join("p2"), data.clone(), |client| {
        call(client, "play.pause", json!({}));
        call(client, "play.step", json!({ "frames": 5 }));
        expect(
            client,
            "master volume persisted",
            json!({ "resource": "GameSettings", "path": "audio.master", "op": "eq", "value": 0.5 }),
        );
        expect(
            client,
            "invert Y persisted",
            json!({ "resource": "GameSettings", "path": "input.invert_y", "op": "eq", "value": true }),
        );
        expect(
            client,
            "sensitivity persisted",
            json!({ "resource": "GameSettings", "path": "input.sensitivity", "op": "eq", "value": 1.5 }),
        );
        let loaded = call(client, "save.load", json!({ "slot": "Chapter1" }));
        assert!(
            loaded["resource_count"].as_u64().is_some_and(|n| n >= 1),
            "{loaded}"
        );
        call(
            client,
            "state.set",
            json!({ "state": "GameFlow", "value": "Loading" }),
        );
        call(client, "play.step", json!({ "frames": 5 }));
        expect(
            client,
            "checkpoint restored",
            summary("checkpoint", "eq", json!(1)),
        );
        expect(client, "defeated restored", summary("defeated", "eq", json!([0])));
        expect(
            client,
            "the defeated grunt stays down",
            summary("enemies_alive", "eq", json!(2)),
        );
    });
    let _ = std::fs::remove_dir_all(&root);
}
