// SPDX-License-Identifier: MIT-0
//! Headless smoke and agent-protocol checks of the Top Down template (`docs/specs/23`).
//!
//! - `headless_smoke_runs_a_fixed_number_of_frames`: the game plugin builds and runs 120 frames
//!   with no window; the level, the player, the enemy (a navmesh agent with a behavior tree) and
//!   the HUD exist afterwards.
//! - `agent_scripted_input_moves_the_character`: `play.inject_input` holds W; the character walks
//!   away from the camera (toward -Z).
//! - `agent_killing_the_enemy_updates_the_hud_and_a_load_brings_it_back`: an attack kills a
//!   weakened enemy, the HUD says so, `save.load` of an earlier save brings it back.
//! - `agent_dying_respawns_at_the_start`: the enemy's blow kills a nearly dead player, who stands
//!   up again at the spawn point with full health.
//!
//! No window and no renderer: `MinimalPlugins` plus the game, a stand-in primary window for the
//! injected input, and a manual 16 ms clock.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use regia::agent::client::Client;
use regia::agent::endpoint;
use regia::prelude::*;
use serde_json::{Value, json};
use {{crate_name}}::Game;

/// A hang guard for the app loop, not a performance budget.
const WATCHDOG: Duration = Duration::from_secs(120);

fn unique_project() -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "{{crate_name}}-agent-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let agent_config = dir.join(".regia");
    std::fs::create_dir_all(&agent_config).expect("project dir");
    // The scripts send many mutating calls per second; the default budget is 50.
    std::fs::write(agent_config.join("agent.toml"), "rate_limit_per_second = 10000\n").expect("agent.toml");
    dir
}

fn headless_app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, Game))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app
}

fn agent_app(project: PathBuf) -> App {
    let mut app = headless_app();
    app.register_type::<regia_render::DirectionalLight>()
        .add_plugins(regia::agent::AgentPlugin)
        .insert_resource(regia::agent::AgentSettings {
            project_dir: Some(project),
        });
    app.world_mut().spawn(regia::platform::PrimaryWindow);
    app
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn wait_until(client: &mut Client, condition: Value) -> Value {
    let report = call(
        client,
        "play.wait_until",
        json!({ "condition": condition, "timeout_ms": 30_000 }),
    );
    assert_eq!(report["satisfied"], true, "wait_until {condition}: {report}");
    report
}

fn key(client: &mut Client, key: &str, state: &str) {
    call(
        client,
        "play.inject_input",
        json!({ "events": [{ "type": "key", "key": key, "state": state }] }),
    );
}

fn set_f32(client: &mut Client, entity: u64, component: &str, path: &str, value: f32) {
    call(
        client,
        "scene.set_property",
        json!({ "entity": entity, "component": component, "path": path, "value": value }),
    );
}

fn character_of(client: &mut Client) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "component": "Character", "limit": 4 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no character: {listed}"))
}

/// Puts the character at rest at `(x, 1.0, z)` with no leftover velocity.
fn place(client: &mut Client, character: u64, x: f32, z: f32) {
    for axis in ["x", "y", "z"] {
        set_f32(
            client,
            character,
            "CharacterState",
            &format!("velocity.{axis}"),
            0.0,
        );
    }
    set_f32(client, character, "Transform", "translation.x", x);
    set_f32(client, character, "Transform", "translation.y", 1.0);
    set_f32(client, character, "Transform", "translation.z", z);
}

/// The enemy line of the HUD (the health line changes with every blow taken).
fn status_is(text: &str) -> Value {
    json!({
        "entity": { "name": "hud-enemy" },
        "component": "EnemyLabel",
        "path": "text",
        "op": "eq",
        "value": text
    })
}

/// Runs `script` on a client thread against the agent app until it finishes.
fn run_with_script(name: &str, script: impl FnOnce(Client) + Send + 'static) {
    let project = unique_project();
    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let client_thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                let token = std::fs::read_to_string(endpoint::agent_dir(&script_project).join("token"));
                if let (Ok((advertised, _)), Ok(token)) = (endpoint::read(&agent_dir), token)
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no agent endpoint at {}", agent_dir.display());
            };
            call(
                &mut client,
                "agent.hello",
                json!({ "client": "template-smoke", "model": "test" }),
            );
            script(client);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");
    let started = Instant::now();
    let mut app = agent_app(project.clone());
    app.finish_plugins();
    while !done.load(Ordering::Relaxed) && started.elapsed() < WATCHDOG {
        app.update();
    }
    let finished = client_thread.join();
    assert!(finished.is_ok(), "the agent script panicked");
    assert!(done.load(Ordering::Relaxed), "the agent script did not finish");
    let _ = std::fs::remove_dir_all(&project);
}

/// Presses `key` until `condition` holds, releasing it between tries. A teleport by
/// `scene.set_property` can still be settling in the physics body when the first press lands
/// (the swing then starts from the old spot), so a press that had no effect is repeated.
fn press_until(client: &mut Client, key_name: &str, condition: Value) {
    for _ in 0..8 {
        key(client, key_name, "pressed");
        let report = call(
            client,
            "play.wait_until",
            json!({ "condition": condition.clone(), "timeout_ms": 1500 }),
        );
        key(client, key_name, "released");
        if report["satisfied"] == true {
            return;
        }
    }
    panic!("pressing {key_name} never produced {condition}");
}

#[test]
fn headless_smoke_runs_a_fixed_number_of_frames() {
    let mut app = headless_app();
    app.finish_plugins();
    for _ in 0..120 {
        app.update();
    }
    let world = app.world_mut();
    let characters = world
        .query_filtered::<Entity, With<regia_framework::character::Character>>()
        .iter(world)
        .count();
    assert_eq!(characters, 1, "one player character");
    let enemies = world
        .query_filtered::<Entity, With<{{crate_name}}::enemy::Enemy>>()
        .iter(world)
        .count();
    assert_eq!(enemies, 1, "the enemy stands on the navmesh");
    let status = world
        .query::<&{{crate_name}}::hud::StatusLabel>()
        .iter(world)
        .map(|label| label.text.clone())
        .collect::<Vec<_>>();
    assert_eq!(status, ["Health: 100 / 100"]);
    let enemy = world
        .query::<&{{crate_name}}::hud::EnemyLabel>()
        .iter(world)
        .map(|label| label.text.clone())
        .collect::<Vec<_>>();
    assert_eq!(enemy, ["Enemy: 36"]);
}

#[test]
fn agent_scripted_input_moves_the_character() {
    run_with_script("template-move", |mut client| {
        let character = character_of(&mut client);
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "CharacterState", "path": "grounded", "op": "eq", "value": true }),
        );
        // W walks away from the camera, toward -Z; the start is z = 0.
        key(&mut client, "KeyW", "pressed");
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "Transform", "path": "translation.z", "op": "lt", "value": -1.5 }),
        );
        key(&mut client, "KeyW", "released");
    });
}

fn enemy_of(client: &mut Client) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "name": "Enemy", "limit": 2 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no enemy: {listed}"))
}

#[test]
fn agent_killing_the_enemy_updates_the_hud_and_a_load_brings_it_back() {
    run_with_script("template-kill", |mut client| {
        wait_until(&mut client, status_is("Enemy: 36"));
        let character = character_of(&mut client);
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "CharacterState", "path": "grounded", "op": "eq", "value": true }),
        );
        let saved = call(&mut client, "save.save", json!({ "slot": "SmokeKill" }));
        assert!(saved["bytes_written"].as_u64().is_some_and(|n| n > 0), "{saved}");

        // The enemy stands at (0, -12). Weaken it, stand 1.5 m from it, attack once.
        let enemy = enemy_of(&mut client);
        set_f32(&mut client, enemy, "Health", "current", 5.0);
        place(&mut client, character, 0.0, -10.5);
        press_until(&mut client, "KeyF", status_is("Enemy: down"));

        // The load gives the enemy back at full health.
        place(&mut client, character, 0.0, 8.0);
        call(&mut client, "save.load", json!({ "slot": "SmokeKill" }));
        wait_until(&mut client, status_is("Enemy: 36"));
    });
}

#[test]
fn agent_dying_respawns_at_the_start() {
    run_with_script("template-death", |mut client| {
        let character = character_of(&mut client);
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "CharacterState", "path": "grounded", "op": "eq", "value": true }),
        );
        // A nearly dead player beside the enemy: its first blow kills.
        set_f32(&mut client, character, "Health", "current", 5.0);
        place(&mut client, character, 0.0, -10.5);
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "Health", "path": "current", "op": "eq", "value": 0.0 }),
        );
        // After the countdown the player stands at the spawn point, healed.
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "Health", "path": "current", "op": "eq", "value": 100.0 }),
        );
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "Transform", "path": "translation.z", "op": "gt", "value": -3.0 }),
        );
    });
}

/// The default sun is visible through the same scene protocol used by editor agents.
#[test]
fn agent_reads_the_shadow_casting_sun() {
    run_with_script("template-sun", |mut client| {
        wait_until(
            &mut client,
            json!({ "entity": { "name": "Sun" }, "component": "DirectionalLight",
                    "path": "cast_shadows", "op": "eq", "value": true }),
        );
        let listed = call(
            &mut client,
            "scene.query_entities",
            json!({ "component": "DirectionalLight", "limit": 4 }),
        );
        let entities = listed["entities"].as_array().expect("sun query rows");
        assert_eq!(entities.len(), 1, "one sun: {listed}");
        let sun = call(
            &mut client,
            "scene.get_entity",
            json!({ "entity": entities[0]["entity"], "components": ["DirectionalLight"] }),
        );
        assert_eq!(
            sun["components"]["DirectionalLight"]["value"]["illuminance"],
            90_000.0
        );
    });
}
