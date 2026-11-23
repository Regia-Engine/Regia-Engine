// SPDX-License-Identifier: MIT-0
//! Headless smoke and agent-protocol checks of the Third Person template (`docs/specs/23`).
//!
//! - `headless_smoke_runs_a_fixed_number_of_frames`: the game plugin builds and runs 120 frames
//!   with no window; the level, the character and the HUD exist afterwards.
//! - `agent_scripted_input_moves_the_character`: `play.inject_input` holds W; the character
//!   walks forward (toward -Z) and ends up on the ground.
//! - `agent_collecting_a_coin_updates_the_hud_and_the_save_restores_it`: stepping onto a coin
//!   changes the HUD counter string, `save.save` / `save.load` bring the coin back, and the
//!   counter follows.
//!
//! - `agent_killing_an_enemy_updates_the_hud_and_the_save_restores_it`: a swing of the sword kills
//!   a weakened grunt, the HUD counts it, and a load brings back a grunt killed after the save.
//! - `agent_dying_shows_the_banner_and_respawns_at_the_start`: a grunt's blow kills a nearly dead
//!   player, "You died" shows, and the player stands up at the spawn point with health.
//! - `title_screen_play_starts_the_game_and_settings_edit_the_saved_settings`: the title shows,
//!   Play starts the game, and the settings panel edits `GameSettings`.
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
use regia_framework::flow::{GameFlow, GameFlowSettings};
use regia_framework::settings::SettingsPath;
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
    // No title screen (the title test builds its own app) and no settings file on disk.
    app.insert_resource(GameFlowSettings {
        skip_main_menu: true,
        ..Default::default()
    })
    .insert_resource(SettingsPath(None));
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

fn hud_is(text: &str) -> Value {
    json!({
        "entity": { "name": "hud-counter" },
        "component": "HudLabel",
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
    let coins = world
        .query_filtered::<Entity, With<{{crate_name}}::collectible::CollectibleId>>()
        .iter(world)
        .count();
    assert_eq!(
        coins,
        {{crate_name}}::collectible::spots().len(),
        "every coin is in the level"
    );
    let counter = world
        .query::<&{{crate_name}}::hud::HudLabel>()
        .iter(world)
        .map(|label| label.text.clone())
        .collect::<Vec<_>>();
    assert_eq!(counter, ["Coins: 0 / 4"]);
}

#[test]
fn agent_scripted_input_moves_the_character() {
    run_with_script("template-move", |mut client| {
        let character = character_of(&mut client);
        wait_until(
            &mut client,
            json!({
                "entity": character,
                "component": "CharacterState",
                "path": "grounded",
                "op": "eq",
                "value": true
            }),
        );
        // W walks away from the camera, toward -Z; the start is z = 5.
        key(&mut client, "KeyW", "pressed");
        wait_until(
            &mut client,
            json!({
                "entity": character,
                "component": "Transform",
                "path": "translation.z",
                "op": "lt",
                "value": 3.0
            }),
        );
        key(&mut client, "KeyW", "released");
    });
}

#[test]
fn agent_collecting_a_coin_updates_the_hud_and_the_save_restores_it() {
    run_with_script("template-coin", |mut client| {
        wait_until(&mut client, hud_is("Coins: 0 / 4"));
        let character = character_of(&mut client);

        // Stand on the first coin: the HUD string changes and the coin is gone.
        place(&mut client, character, 0.0, -3.0);
        wait_until(&mut client, hud_is("Coins: 1 / 4"));

        let saved = call(&mut client, "save.save", json!({ "slot": "Smoke" }));
        assert!(saved["bytes_written"].as_u64().is_some_and(|n| n > 0), "{saved}");

        // A second coin after the save...
        place(&mut client, character, -6.0, -5.0);
        wait_until(&mut client, hud_is("Coins: 2 / 4"));

        // ...is given back by the load, and so is the counter. Step off the coin first.
        place(&mut client, character, -10.0, 10.0);
        call(&mut client, "save.load", json!({ "slot": "Smoke" }));
        wait_until(&mut client, hud_is("Coins: 1 / 4"));
        let coins = call(
            &mut client,
            "scene.query_entities",
            json!({ "component": "CollectibleId", "limit": 10 }),
        );
        let alive = coins["entities"].as_array().map_or(0, Vec::len);
        assert_eq!(alive, 3, "three coins remain after the load: {coins}");
    });
}

/// The enemy-count line of the HUD (the health line changes with every blow taken).
fn combat_is(text: &str) -> Value {
    json!({
        "entity": { "name": "hud-enemies" },
        "component": "EnemiesLabel",
        "path": "text",
        "op": "eq",
        "value": text
    })
}

fn banner_is(text: &str) -> Value {
    json!({
        "entity": { "name": "hud-banner" },
        "component": "BannerLabel",
        "path": "text",
        "op": "eq",
        "value": text
    })
}

/// The agent entity id of grunt `id` (they are named `Grunt<id>`).
fn enemy_of(client: &mut Client, id: u64) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "name": format!("Grunt{id}"), "limit": 4 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no grunt {id}: {listed}"))
}

fn enemy_count(client: &mut Client) -> usize {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "component": "EnemyId", "limit": 8 }),
    );
    listed["entities"].as_array().map_or(0, Vec::len)
}

/// Stands the player at `(x, 1.0, z)` and swings at a grunt weakened to 5 hit points until
/// `condition` (the HUD saying it fell) holds.
fn weaken_and_strike(client: &mut Client, character: u64, enemy: u64, spot: (f32, f32), condition: Value) {
    set_f32(client, enemy, "Health", "current", 5.0);
    place(client, character, spot.0, spot.1);
    press_until(client, "KeyJ", condition);
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
fn agent_killing_an_enemy_updates_the_hud_and_the_save_restores_it() {
    run_with_script("template-kill", |mut client| {
        wait_until(&mut client, combat_is("Enemies: 0 / 3"));
        let character = character_of(&mut client);
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "CharacterState", "path": "grounded", "op": "eq", "value": true }),
        );

        // Grunt 0 stands at (-5, -9). The player faces -Z, 1.5 m in front of it.
        let first = enemy_of(&mut client, 0);
        weaken_and_strike(
            &mut client,
            character,
            first,
            (-5.0, -7.5),
            combat_is("Enemies: 1 / 3"),
        );

        let saved = call(&mut client, "save.save", json!({ "slot": "SmokeKill" }));
        assert!(saved["bytes_written"].as_u64().is_some_and(|n| n > 0), "{saved}");

        // Grunt 1 stands at (5, -10).
        let second = enemy_of(&mut client, 1);
        weaken_and_strike(
            &mut client,
            character,
            second,
            (5.0, -8.5),
            combat_is("Enemies: 2 / 3"),
        );

        // The load gives grunt 1 back and keeps grunt 0 dead.
        place(&mut client, character, 0.0, 8.0);
        call(&mut client, "save.load", json!({ "slot": "SmokeKill" }));
        wait_until(&mut client, combat_is("Enemies: 1 / 3"));
        let mut alive = 0;
        for _ in 0..200 {
            alive = enemy_count(&mut client);
            if alive == 2 {
                break;
            }
        }
        assert_eq!(alive, 2, "two grunts stand after the load");
    });
}

#[test]
fn agent_dying_shows_the_banner_and_respawns_at_the_start() {
    run_with_script("template-death", |mut client| {
        let character = character_of(&mut client);
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "CharacterState", "path": "grounded", "op": "eq", "value": true }),
        );
        // A nearly dead player beside grunt 2 (at (-9, 6)): its first blow kills.
        set_f32(&mut client, character, "Health", "current", 5.0);
        place(&mut client, character, -8.0, 6.0);
        wait_until(&mut client, banner_is("You died"));
        // After the countdown the banner clears and the player stands at the spawn point, healed.
        wait_until(&mut client, banner_is(""));
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "Health", "path": "current", "op": "gte", "value": 90.0 }),
        );
        wait_until(
            &mut client,
            json!({ "entity": character, "component": "Transform", "path": "translation.x", "op": "gt", "value": -4.0 }),
        );
    });
}

#[test]
fn title_screen_play_starts_the_game_and_settings_edit_the_saved_settings() {
    let mut app = App::new();
    app.insert_resource(SettingsPath(None));
    app.add_plugins((MinimalPlugins, Game))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.finish_plugins();
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        *app.world().resource::<State<GameFlow>>().get(),
        GameFlow::MainMenu
    );
    assert_eq!(
        app.world().resource::<{{crate_name}}::menu::MenuState>().screen,
        "title"
    );

    // Settings open over the title, edit the live settings, and close.
    {{crate_name}}::menu::apply_action(app.world_mut(), {{crate_name}}::menu::MenuAction::OpenSettings);
    {{crate_name}}::menu::apply_action(app.world_mut(), {{crate_name}}::menu::MenuAction::Master(false));
    app.update();
    assert_eq!(
        app.world().resource::<{{crate_name}}::menu::MenuState>().screen,
        "settings"
    );
    let master = app
        .world()
        .resource::<regia_framework::settings::GameSettings>()
        .audio
        .master;
    assert!(master < 1.0, "master volume went down: {master}");
    {{crate_name}}::menu::apply_action(app.world_mut(), {{crate_name}}::menu::MenuAction::CloseSettings);

    // Play leaves the title; a few frames later the game is in `Playing`.
    {{crate_name}}::menu::apply_action(app.world_mut(), {{crate_name}}::menu::MenuAction::Play);
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(
        *app.world().resource::<State<GameFlow>>().get(),
        GameFlow::Playing
    );
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
