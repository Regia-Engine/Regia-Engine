// SPDX-License-Identifier: MIT-0
//! Headless smoke and agent-protocol checks of the First Person template (`docs/specs/23`).
//!
//! - `headless_smoke_runs_a_fixed_number_of_frames`: the game plugin builds and runs 120 frames
//!   with no window; the player, the camera, the gate, the button and the crosshair exist.
//! - `agent_scripted_input_moves_the_player`: `play.inject_input` holds W; the pawn walks toward -Z.
//! - `agent_using_the_gate_shows_the_prompt_and_opens_it`: standing in front of the gate shows its
//!   prompt; E opens it (the prompt changes); a load of an earlier save closes it again.
//! - `pause_freezes_time_and_resume_restores_it`: the pause flow stops gameplay time and shows the
//!   menu.
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
    // No settings file on disk.
    app.insert_resource(regia_framework::settings::SettingsPath(None));
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

fn prompt_is(text: &str) -> Value {
    json!({
        "entity": { "name": "hud-prompt" },
        "component": "PromptLabel",
        "path": "text",
        "op": "eq",
        "value": text
    })
}

fn door_is(open: bool) -> Value {
    json!({
        "entity": { "name": "Gate" },
        "component": "Door",
        "path": "open",
        "op": "eq",
        "value": open
    })
}

/// The pawn's entity id.
fn player_of(client: &mut Client) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "name": "player", "limit": 2 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no player: {listed}"))
}

/// Puts the pawn at `(x, 1.6, z)`; the view yaw stays 0, looking toward -Z.
fn place(client: &mut Client, player: u64, x: f32, z: f32) {
    set_f32(client, player, "Transform", "translation.x", x);
    set_f32(client, player, "Transform", "translation.y", 1.6);
    set_f32(client, player, "Transform", "translation.z", z);
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

#[test]
fn headless_smoke_runs_a_fixed_number_of_frames() {
    let mut app = headless_app();
    app.finish_plugins();
    for _ in 0..120 {
        app.update();
    }
    let world = app.world_mut();
    assert_eq!(
        world.query::<&{{crate_name}}::Player>().iter(world).count(),
        1,
        "one player"
    );
    assert_eq!(
        world
            .query::<&{{crate_name}}::interact::Door>()
            .iter(world)
            .count(),
        1,
        "the gate"
    );
    assert_eq!(
        world
            .query::<&{{crate_name}}::interact::Button>()
            .iter(world)
            .count(),
        1,
        "the button"
    );
    assert_eq!(
        world
            .query::<&{{crate_name}}::hud::Crosshair>()
            .iter(world)
            .count(),
        1,
        "the crosshair"
    );
    let prompts = world
        .query::<&{{crate_name}}::hud::PromptLabel>()
        .iter(world)
        .map(|label| label.text.clone())
        .collect::<Vec<_>>();
    assert_eq!(prompts, [""], "nothing in reach at the start");
}

#[test]
fn agent_scripted_input_moves_the_player() {
    run_with_script("template-move", |mut client| {
        let player = player_of(&mut client);
        // W walks forward, toward -Z; the start is z = 4 and the gate wall is at z = -4.
        key(&mut client, "KeyW", "pressed");
        wait_until(
            &mut client,
            json!({ "entity": player, "component": "Transform", "path": "translation.z", "op": "lt", "value": 1.0 }),
        );
        key(&mut client, "KeyW", "released");
    });
}

#[test]
fn agent_using_the_gate_shows_the_prompt_and_opens_it() {
    run_with_script("template-gate", |mut client| {
        let player = player_of(&mut client);
        wait_until(&mut client, prompt_is(""));

        // Two meters in front of the gate, looking at it.
        place(&mut client, player, 0.0, -2.0);
        wait_until(&mut client, prompt_is("Press E to open the gate"));
        let saved = call(&mut client, "save.save", json!({ "slot": "SmokeGate" }));
        assert!(saved["bytes_written"].as_u64().is_some_and(|n| n > 0), "{saved}");

        key(&mut client, "KeyE", "pressed");
        wait_until(&mut client, door_is(true));
        key(&mut client, "KeyE", "released");
        wait_until(&mut client, prompt_is("Press E to close the gate"));

        // Walk away: the prompt goes. The load closes the gate again.
        place(&mut client, player, 0.0, 6.0);
        wait_until(&mut client, prompt_is(""));
        call(&mut client, "save.load", json!({ "slot": "SmokeGate" }));
        wait_until(&mut client, door_is(false));
    });
}

#[test]
fn pause_freezes_time_and_resume_restores_it() {
    use regia_framework::flow::GameFlow;
    let mut app = headless_app();
    app.finish_plugins();
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(
        *app.world().resource::<State<GameFlow>>().get(),
        GameFlow::Playing
    );
    app.world_mut()
        .resource_mut::<NextState<GameFlow>>()
        .set(GameFlow::Paused);
    for _ in 0..3 {
        app.update();
    }
    assert!(
        app.world().resource::<Time>().paused,
        "paused flow stops gameplay time"
    );
    app.world_mut()
        .resource_mut::<NextState<GameFlow>>()
        .set(GameFlow::Playing);
    for _ in 0..3 {
        app.update();
    }
    assert!(!app.world().resource::<Time>().paused);
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
