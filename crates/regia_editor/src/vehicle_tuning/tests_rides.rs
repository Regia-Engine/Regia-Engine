// SPDX-License-Identifier: MPL-2.0
//! Bike and hover-hull tuning and the proving ground's live bike and hover craft (ADR-8630).
//! Counted fixed steps, no clock.

use std::path::Path;

use regia::framework::det_hover::HoverHullAsset;
use regia::physics::vehicle_bike::asset::BikeAsset;
use regia::physics::vehicle_bike::{BikeConfig, BikeInput, BikeState, FIXED_STEP_S, bike_step};
use regia_detsim::hover::{FlightMode, HoverLimits, HoverParams, HoverState, WorldPhysics, step_with};
use regia_detsim::tick::tick_dt;
use serde_json::{Value, json};

use super::*;

const BIKE_PATH: &str = "assets/city.bike.ron";
const HULL_PATH: &str = "assets/scout.hoverhull.ron";

fn project(tag: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets")).expect("assets dir");
    dir
}

fn world_with_project(dir: &Path) -> World {
    let mut world = World::new();
    world.insert_resource(crate::agent_ext::EditorProject {
        root: Some(dir.to_path_buf()),
    });
    world
}

fn reason(e: &RpcError) -> Option<String> {
    e.data
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .map(str::to_string)
}

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    let list = methods();
    let handler = list
        .iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| *h)
        .unwrap_or_else(|| panic!("{name} is not registered"));
    handler(world, &params)
}

/// The bike the proving ground steps: flat ground, both rays at 0.3 m.
fn reference_bike(config: &BikeConfig, state: &mut BikeState, steps: u32, steer: f32, speed: f32) {
    let input = BikeInput {
        dt: FIXED_STEP_S,
        steer,
        speed,
        drive: 1.0,
        front_ray: 0.3,
        rear_ray: 0.3,
    };
    for _ in 0..steps {
        bike_step(state, config, &input).expect("reference step");
    }
}

/// The hover craft the proving ground steps: a flat floor at 0, Earth, the default limits and the
/// default flight mode.
fn reference_hover(params: &HoverParams, state: &mut HoverState, steps: u32, input: [f32; 3]) {
    let params = params.sanitized();
    let limits = HoverLimits::DEFAULT.sanitized();
    let climb = FlightMode::default().climb();
    for _ in 0..steps {
        step_with(
            &params,
            &limits,
            climb,
            &WorldPhysics::EARTH,
            state,
            &input,
            0.0,
            tick_dt(),
        );
    }
}

fn spawned_hover(params: &HoverParams) -> HoverState {
    HoverState {
        altitude: params.ride_height,
        prev_terrain: Some(0.0),
        ..HoverState::default()
    }
}

/// Acceptance (item 2): a bike field set through `editor.vehicle_tuning_set` reaches the bike already
/// riding on the proving ground at its next step, without a respawn: the live state matches a bike
/// stepped 40 steps on the old configuration and 40 on the new one, bit for bit.
#[test]
fn tuning_a_bike_field_reaches_the_live_proving_ground_bike() {
    let dir = project("vt-bike-live");
    let mut world = world_with_project(&dir);
    let v = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": BIKE_PATH, "create": true }),
    )
    .expect("create");
    assert_eq!(v["opened"], "bike");
    assert_eq!(v["bike"]["name"], "city");
    let ride = json!({ "asset": "bike", "steps": 40, "steer": 0.15, "speed": 9.0 });
    let first = call(&mut world, "editor.vehicle_tuning_ride", ride.clone()).expect("ride");
    assert_eq!(first["asset"], "bike");
    assert_eq!(first["steps"], 40);
    assert_eq!(first["total_steps"], 40);
    assert_eq!(first["spawned"], true);

    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "bike", "field": "lean_response", "value": 9.0 }),
    )
    .expect("lean_response");
    let second = call(&mut world, "editor.vehicle_tuning_ride", ride).expect("ride on");
    assert_eq!(second["total_steps"], 80, "the same bike rode on");
    assert_eq!(second["spawned"], false, "no respawn");
    assert_eq!(second["config"]["lean_response"], 9.0);

    let mut state = BikeState::default();
    let old = BikeConfig::default();
    reference_bike(&old, &mut state, 40, 0.15, 9.0);
    let after_old = state;
    let new = BikeConfig {
        lean_response: 9.0,
        ..old
    };
    reference_bike(&new, &mut state, 40, 0.15, 9.0);
    assert_eq!(second["digest"], format!("{:016x}", state.digest()));
    assert_eq!(second["state"]["lean"], f64::from(state.lean));
    // And the edit mattered: forty more steps on the old configuration end elsewhere.
    let mut unchanged = after_old;
    reference_bike(&old, &mut unchanged, 40, 0.15, 9.0);
    assert_ne!(unchanged.digest(), state.digest());

    // Undo reaches it too, and a respawn starts upright from step 0.
    call(
        &mut world,
        "editor.vehicle_tuning_undo",
        json!({ "asset": "bike" }),
    )
    .expect("undo");
    let third = call(
        &mut world,
        "editor.vehicle_tuning_ride",
        json!({ "asset": "bike", "steps": 1, "respawn": true }),
    )
    .expect("respawn");
    assert_eq!(third["total_steps"], 1);
    assert_eq!(third["spawned"], true);
    assert_eq!(third["config"]["lean_response"], 4.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance (item 2): the same for a hover hull: a `params` field set through the methods reaches
/// the hover craft riding on the proving ground at its next tick.
#[test]
fn tuning_a_hover_hull_field_reaches_the_live_proving_ground_hover_craft() {
    let dir = project("vt-hover-live");
    let mut world = world_with_project(&dir);
    let v = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": HULL_PATH, "create": true }),
    )
    .expect("create");
    assert_eq!(v["opened"], "hoverhull");
    assert_eq!(v["hoverhull"]["name"], "scout");
    let ride = json!({ "asset": "hoverhull", "steps": 45, "throttle": 1.0, "steer": 0.25 });
    let first = call(&mut world, "editor.vehicle_tuning_ride", ride.clone()).expect("ride");
    assert_eq!(first["asset"], "hoverhull");
    assert_eq!(first["total_steps"], 45);

    let defaults = HoverParams::DEFAULT_HULL;
    let faster = defaults.thrust_accel * 1.5;
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "hoverhull", "field": "thrust_accel", "value": faster }),
    )
    .expect("thrust_accel");
    let second = call(&mut world, "editor.vehicle_tuning_ride", ride).expect("ride on");
    assert_eq!(second["total_steps"], 90);
    assert_eq!(second["spawned"], false);

    let mut state = spawned_hover(&defaults);
    reference_hover(&defaults, &mut state, 45, [1.0, 0.0, 0.25]);
    let after_old = state;
    let tuned = HoverParams {
        thrust_accel: faster,
        ..defaults
    };
    reference_hover(&tuned, &mut state, 45, [1.0, 0.0, 0.25]);
    assert_eq!(second["state"]["velocity"][2], f64::from(state.velocity[2]));
    assert_eq!(second["state"]["yaw"], f64::from(state.yaw));
    assert_eq!(second["state"]["altitude"], f64::from(state.altitude));
    let mut unchanged = after_old;
    reference_hover(&defaults, &mut unchanged, 45, [1.0, 0.0, 0.25]);
    assert_ne!(unchanged.velocity, state.velocity, "the edit changed the ride");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bike and a hover hull save through the same methods as a trailer: the canonical writer's bytes
/// for a new file, comments kept on an edited one, and a refused value names its field.
#[test]
fn bike_and_hover_hull_documents_save_undo_and_refuse() {
    let dir = project("vt-bike-hover-save");
    let mut world = world_with_project(&dir);
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": BIKE_PATH, "create": true }),
    )
    .expect("create bike");
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "bike", "field": "wheelbase", "value": 1.4 }),
    )
    .expect("wheelbase");
    let refused = call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "bike", "field": "max_lean", "value": 9.0 }),
    )
    .expect_err("max_lean out of range");
    assert_eq!(reason(&refused).as_deref(), Some("bike_bad_input"));
    let state = call(&mut world, "editor.vehicle_tuning_state", Value::Null).expect("state");
    assert_eq!(state["bike"]["error"]["field"], "max_lean");
    assert_eq!(
        state["bike"]["config"]["max_lean"],
        f64::from(1.05_f32),
        "a refused value changes nothing"
    );
    let bad = call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "bike", "field": "frame", "value": 1.0 }),
    )
    .expect_err("no such field");
    assert_eq!(reason(&bad).as_deref(), Some("vehicle_tuning_bad_field"));
    let saved = call(
        &mut world,
        "editor.vehicle_tuning_save",
        json!({ "asset": "bike" }),
    )
    .expect("save bike");
    assert_eq!(saved["asset"], "bike");
    let text = std::fs::read_to_string(dir.join(BIKE_PATH)).expect("bike file");
    let asset = BikeAsset::from_ron(&text).expect("reload");
    assert_eq!(asset.config.wheelbase, 1.4);
    assert_eq!(
        text,
        asset.to_ron().expect("canonical"),
        "the canonical writer's bytes"
    );

    // A hand-written hull with a comment: the edit keeps it.
    let hull_text = "// scout hull, tuned by hand\n(\n    format_version: 1,\n    name: \"scout\",\n    params: (ride_height: 1.5),\n)\n";
    std::fs::write(dir.join(HULL_PATH), hull_text).expect("hull file");
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": HULL_PATH }),
    )
    .expect("open hull");
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "hoverhull", "field": "ride_height", "value": 2.0 }),
    )
    .expect("ride_height");
    let refused = call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "hoverhull", "field": "ride_height", "value": -1.0 }),
    )
    .expect_err("negative ride height");
    let code = reason(&refused).unwrap_or_default();
    assert!(
        ["negative", "out_of_range"].contains(&code.as_str()),
        "hover field code, got {code}"
    );
    let saved = call(
        &mut world,
        "editor.vehicle_tuning_save",
        json!({ "asset": "hoverhull" }),
    )
    .expect("save hull");
    let text = std::fs::read_to_string(dir.join(HULL_PATH)).expect("hull file");
    let hull = HoverHullAsset::from_ron(&text).expect("reload hull");
    assert_eq!(hull.params.ride_height, 2.0);
    if saved["comments_kept"] == true {
        assert!(text.contains("tuned by hand"), "{text}");
    }
    let v = call(
        &mut world,
        "editor.vehicle_tuning_undo",
        json!({ "asset": "hoverhull" }),
    )
    .expect("undo");
    assert_eq!(v["changed"], true);
    assert_eq!(v["hoverhull"]["config"]["ride_height"], 1.5);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The ride refuses what it cannot step, by code, and changes nothing.
#[test]
fn a_ride_refuses_by_code() {
    let dir = project("vt-ride-refuse");
    let mut world = world_with_project(&dir);
    let nothing = call(
        &mut world,
        "editor.vehicle_tuning_ride",
        json!({ "asset": "bike", "steps": 10 }),
    )
    .expect_err("no bike open");
    assert_eq!(reason(&nothing).as_deref(), Some("vehicle_tuning_nothing_open"));
    let car = call(
        &mut world,
        "editor.vehicle_tuning_ride",
        json!({ "asset": "trailer", "steps": 10 }),
    )
    .expect_err("a trailer does not ride");
    assert_eq!(reason(&car).as_deref(), Some("vehicle_tuning_bad_asset"));
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": BIKE_PATH, "create": true }),
    )
    .expect("create");
    for steps in [0, 7201] {
        let e = call(
            &mut world,
            "editor.vehicle_tuning_ride",
            json!({ "asset": "bike", "steps": steps }),
        )
        .expect_err("steps out of range");
        assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_steps"));
    }
    let e = call(
        &mut world,
        "editor.vehicle_tuning_ride",
        json!({ "asset": "bike", "steps": 5, "steer": 3.0 }),
    )
    .expect_err("steer past the model's range");
    assert_eq!(reason(&e).as_deref(), Some("bike_bad_input"));
    let state = call(&mut world, "editor.vehicle_tuning_state", Value::Null).expect("state");
    assert!(
        state["rides"]["bike"].is_null(),
        "a refused first ride spawns nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bike and hover-hull sections draw headless with a ride recorded, and drawing changes nothing.
#[test]
fn the_panel_draws_the_bike_and_hover_sections_headless() {
    let dir = project("vt-bike-hover-draw");
    let mut world = world_with_project(&dir);
    for (path, asset) in [(BIKE_PATH, "bike"), (HULL_PATH, "hoverhull")] {
        call(
            &mut world,
            "editor.vehicle_tuning_open",
            json!({ "path": path, "create": true }),
        )
        .expect(path);
        call(
            &mut world,
            "editor.vehicle_tuning_ride",
            json!({ "asset": asset, "steps": 30, "speed": 6.0, "throttle": 1.0 }),
        )
        .expect("ride");
    }
    let ctx = egui::Context::default();
    for _ in 0..2 {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(520.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            crate::vehicle_tuning_panel::vehicle_tuning_panel(ui, &mut world)
        });
        output.textures_delta.clear();
        assert!(!ctx.tessellate(output.shapes, output.pixels_per_point).is_empty());
    }
    let panel = world.resource::<VehicleTuning>();
    assert_eq!(panel.others.bike.undo.len(), 0, "drawing changes nothing");
    assert_eq!(panel.others.hoverhull.undo.len(), 0, "drawing changes nothing");
    assert_eq!(panel.others.bike_ride.as_ref().map(|r| r.steps), Some(30));
    assert_eq!(panel.others.hover_ride.as_ref().map(|r| r.steps), Some(30));
    let _ = std::fs::remove_dir_all(&dir);
}
