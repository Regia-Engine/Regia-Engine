// SPDX-License-Identifier: MIT-0
//! General-engine acceptance: one saved navigation route, demo seeks, and spectator input.
//! The agent case uses exact frame requests; socket scheduling never advances gameplay.

use std::path::{Path, PathBuf};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::demo::{DemoHeader, DemoRecorder, DemoRecording, DemoSim};
use regia_framework::demo_world::{DemoTracked, DemoWorldPlugin, WorldDemoSim, world_demo_hash};
use regia_framework::save_flow::{SaveFlowPlugin, load_from_store, save_to_store};
use regia_framework::save_game::{SaveDirectory, SavePlugin, Saveable};
use regia_nav::{NavAgent, NavAgentState, NavMeshBuilder, NavObstacles, NavPlugin, NavQuery};
use regia_scene::Transform;
use regia_scene::components::Name;
use serde_json::{Value, json};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        Self(regia_test_utils::unique_temp_dir("general-workflow"))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn app(project: &Path) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        NavPlugin,
        SavePlugin::default(),
        SaveFlowPlugin,
        DemoWorldPlugin,
    ))
    .register_type::<Name>()
    .register_type::<Transform>()
    .insert_resource(SaveDirectory(project.join("saves")));
    enable_fixed_step(&mut app, 60.0);

    let corners = [
        Vec3::new(-8.0, 0.0, -6.0),
        Vec3::new(8.0, 0.0, -6.0),
        Vec3::new(8.0, 0.0, 6.0),
        Vec3::new(-8.0, 0.0, 6.0),
    ];
    let mesh = NavMeshBuilder::default()
        .build(&[
            [corners[0], corners[1], corners[2]],
            [corners[0], corners[2], corners[3]],
        ])
        .expect("lab floor");
    let mut obstacles = NavObstacles::default();
    obstacles.add_box(Vec3::new(-1.0, -1.0, -2.0), Vec3::new(1.0, 2.0, 2.0));
    let start = Vec3::new(-6.0, 0.0, 0.0);
    let goal = Vec3::new(6.0, 0.0, 0.0);
    let path = NavQuery {
        mesh: &mesh,
        obstacles: Some(&obstacles),
    }
    .find_path(start, goal, Vec3::splat(2.0))
    .expect("route around the obstacle");
    assert!(path.waypoints.iter().any(|p| p.z.abs() >= 2.0));
    let mut state = NavAgentState::default();
    state.set_path(path);
    app.insert_resource(mesh).insert_resource(obstacles);
    // No platform inputs: the recorded action list is empty. The fixed navigation state is
    // the simulation; the unchanged mesh and obstacle set are level content shared by replays.
    app.world_mut().spawn((
        Name::new("Walker"),
        DemoTracked,
        Saveable,
        Transform::from_translation(start),
        NavAgent::default(),
        state,
    ));
    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    app
}

fn walker(app: &mut App) -> Entity {
    app.world_mut()
        .query::<(Entity, &Name)>()
        .iter(app.world())
        .find_map(|(entity, name)| (name.as_str() == "Walker").then_some(entity))
        .expect("walker exists")
}

fn position(app: &mut App) -> Vec3 {
    let entity = walker(app);
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

#[test]
fn a_saved_route_resumes_and_every_demo_seek_matches_the_original() {
    let project = Project::new();
    let mut app = app(&project.0);
    app.world_mut().resource_mut::<Time>().paused = false;
    for _ in 0..30 {
        app.update();
    }
    let saved_entity = walker(&mut app);
    let saved_position = position(&mut app);
    let saved_hash = world_demo_hash(app.world());
    save_to_store(app.world_mut(), "Route").expect("atomic file save");

    let header = DemoHeader::new("nav_world_lab/workflow", 17, 60, 29);
    let mut sim = WorldDemoSim::new(app.world_mut(), vec![]);
    let mut recorder = DemoRecorder::begin(header, vec![], &sim);
    let mut hashes = vec![sim.state_hash()];
    for _ in 0..360 {
        recorder.record_step(&mut sim, &[]).expect("navigation step");
        hashes.push(sim.state_hash());
    }
    let recording = recorder.finish(&sim);
    let end = position(&mut app);
    assert!(end.distance(Vec3::new(6.0, 0.0, 0.0)) < 0.5, "arrived: {end:?}");
    let path = project.0.join("route.regiademo");
    recording.save(&path).expect("atomic demo write");
    let recording = DemoRecording::load(&path).expect("demo read");

    load_from_store(app.world_mut(), "Route").expect("load saved route");
    assert_ne!(walker(&mut app), saved_entity, "load respawns saveable entities");
    assert_eq!(position(&mut app), saved_position);
    assert_eq!(
        world_demo_hash(app.world()),
        saved_hash,
        "all route state restored"
    );
    for (step, expected) in hashes.iter().enumerate().skip(1) {
        app.update();
        assert_eq!(
            world_demo_hash(app.world()),
            *expected,
            "loaded continuation {step}"
        );
    }
    // Seek in both directions, at keyframes and between them; entity ids changed on load.
    for tick in [0, 1, 28, 29, 30, 121, 359, 360, 17, 247] {
        let report = recording
            .seek(&mut WorldDemoSim::new(app.world_mut(), vec![]), tick)
            .expect("seek a saved navigation demo");
        assert_eq!(report.state_hash, hashes[tick as usize], "seek {tick}");
    }
    recording
        .verify(&mut WorldDemoSim::new(app.world_mut(), vec![]))
        .expect("all replay keyframes verified");
}

fn call(client: &mut regia_agent::client::Client, method: &str, params: Value) -> Value {
    client
        .call(method, params)
        .unwrap_or_else(|error| panic!("{method}: {error}"))
}

fn condition(x: f32) -> Value {
    json!({"entity": {"name": "Walker"}, "component": "Transform",
           "path": "translation.x", "op": "gt", "value": x})
}

#[test]
fn saved_navigation_and_spectator_are_observable_through_the_agent() {
    let project = Project::new();
    let mut app = app(&project.0);
    app.insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project.0.clone()),
    })
    .add_plugins(regia_agent::AgentPlugin);
    app.finish_plugins();
    // finish_plugins publishes a listening endpoint synchronously. No sleep or endpoint poll.
    let dir = regia_agent::endpoint::agent_dir(&project.0);
    let (endpoint, _) = regia_agent::endpoint::read(&dir).expect("published endpoint");
    let token = std::fs::read_to_string(dir.join("token")).expect("local test token");
    let original_hash = world_demo_hash(app.world());
    let script = std::thread::spawn(move || {
        let mut client = regia_agent::client::Client::connect(&endpoint.url, &token).expect("connect");
        call(&mut client, "save.save", json!({"slot": "Route"}));
        call(&mut client, "play.step", json!({"frames": 60}));
        let moved = call(
            &mut client,
            "play.expect",
            json!({"assertions": [
                {"name": "walker advanced", "condition": condition(-5.0)}
            ]}),
        );
        assert_eq!(moved["pass"], true, "{moved}");
        let waited = call(
            &mut client,
            "play.wait_until",
            json!({"condition": condition(-5.0)}),
        );
        assert_eq!(waited["satisfied"], true, "{waited}");
        call(&mut client, "save.load", json!({"slot": "Route"}));
        let restored = call(
            &mut client,
            "play.expect",
            json!({"assertions": [
                {"name": "saved position", "condition": {
                    "entity": {"name": "Walker"}, "component": "Transform",
                    "path": "translation.x", "op": "eq", "value": -6.0
                }}
            ]}),
        );
        assert_eq!(restored["pass"], true, "{restored}");
        call(&mut client, "spectator.set", json!({"position": [0, 5, 10]}));
        let camera = call(
            &mut client,
            "spectator.step",
            json!({
                "frames": 60, "dt": 0.016, "movement": [1, 0, 0]
            }),
        );
        assert!(
            camera["position"][0].as_f64().is_some_and(|x| x > 1.0),
            "{camera}"
        );
        call(&mut client, "save.delete", json!({"slot": "Route"}));
    });
    // The app is paused except for play.step's exact 60 frames. Scheduling the client can only
    // add paused presentation frames; it cannot affect saved state or simulation assertions.
    while !script.is_finished() {
        app.update();
        std::thread::yield_now();
    }
    script.join().expect("agent assertions");
    assert_eq!(
        world_demo_hash(app.world()),
        original_hash,
        "spectator did not move simulation"
    );
}
