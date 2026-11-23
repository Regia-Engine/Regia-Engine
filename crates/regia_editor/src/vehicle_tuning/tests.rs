// SPDX-License-Identifier: MPL-2.0
//! The Vehicle Tuning panel's acceptance (ADR-9760): open the vehicle lab's car, raise its final
//! drive by 20 %, Apply in a play world and measure 0 to 100 km/h there; plots against the runtime;
//! save then reload; refusals change nothing; undo per edit and per drag; the agent methods by name.

use std::path::Path;
use std::time::Duration;

use regia::physics::vehicle::{VehicleConfig, VehicleController, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia::physics::vehicle_asset::VehicleAsset;
use regia::physics::vehicle_proving::{ProvingScript, add_proving_ground, spawn_height};
use regia::physics::{PhysicsPlugin, PhysicsWorld};
use regia_app::prelude::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use serde_json::{Value, json};

use super::*;

/// The vehicle lab's car, as the sample ships it (a v2 file with comments).
const LAB_CAR: &str = include_str!("../../../../samples/vehicle_lab/assets/rwd_car.vehicle.ron");

const CAR_PATH: &str = "assets/rwd_car.vehicle.ron";

fn project_with_lab_car(tag: &str) -> std::path::PathBuf {
    let dir = regia_test_utils::unique_temp_dir(tag);
    std::fs::create_dir_all(dir.join("assets")).expect("assets dir");
    std::fs::write(dir.join(CAR_PATH), LAB_CAR).expect("car file");
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

fn final_drive(panel: &VehicleTuning) -> f64 {
    let asset = panel.asset.as_ref().expect("open");
    read_field(&asset.config, "drivetrain.gearbox.final_drive")
        .and_then(|v| v.as_f64())
        .expect("final drive")
}

/// A play world like a game's: the physics and vehicle plugins, the proving ground's plane, the
/// car spawned from `config`. Steps at 60 Hz by a manual clock.
fn play_app(config: VehicleConfig) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins((regia_scene::ScenePlugin, PhysicsPlugin, VehiclePlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    app.update();
    add_proving_ground(&mut app.world_mut().resource_mut::<PhysicsWorld>());
    let height = spawn_height(&config) + 0.02;
    let car = spawn_vehicle(
        app.world_mut(),
        config,
        Vec3::new(0.0, height, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    (app, car)
}

/// Fixed steps the car has taken.
fn steps(app: &App, car: Entity) -> u64 {
    app.world()
        .get::<VehicleController>(car)
        .map_or(0, |c| c.vehicle.steps())
}

/// Updates until the car has taken `n` more fixed steps.
fn advance(app: &mut App, car: Entity, n: u64) {
    let target = steps(app, car) + n;
    for _ in 0..(n * 4 + 8) {
        if steps(app, car) >= target {
            return;
        }
        app.update();
    }
    panic!("the play world did not step");
}

/// Settles, applies (or not) through the panel's pump, then counts fixed steps at full throttle to
/// 100 km/h.
fn play_zero_to_100(panel: &mut VehicleTuning, apply: bool) -> (u64, Value) {
    let config = VehicleAsset::from_ron(LAB_CAR).expect("lab").into_config();
    let (mut app, car) = play_app(config);
    advance(&mut app, car, 60);
    let mut answer = Value::Null;
    if apply {
        panel.pending_apply = Some(None);
        pump_world(panel, Some(app.world_mut()));
        let applied = panel.last_apply.clone().expect("apply ran").expect("apply ok");
        assert_eq!(applied["entity"], json!(car.to_bits()));
        answer = regia_agent::panel_calls::call(app.world_mut(), "physics.vehicles", &json!({}))
            .expect("vehicles");
    }
    if let Some(mut input) = app.world_mut().get_mut::<VehicleInput>(car) {
        input.throttle = 1.0;
    }
    let start = steps(&app, car);
    for _ in 0..60 * 30 {
        advance(&mut app, car, 1);
        pump_world(panel, Some(app.world_mut()));
        let physics = app.world().resource::<PhysicsWorld>();
        let speed = app
            .world()
            .get::<VehicleController>(car)
            .and_then(|c| c.vehicle.state(physics).ok())
            .map_or(0.0, |s| s.forward_speed);
        if speed >= 100.0 / 3.6 {
            return (steps(&app, car) - start, answer);
        }
    }
    panic!("the play car never reached 100 km/h");
}

#[test]
fn raising_the_final_drive_and_applying_in_play_moves_the_zero_to_100_time() {
    let dir = project_with_lab_car("vehicle-tuning-accept");
    let mut panel = VehicleTuning::default();
    panel.open(Some(&dir), CAR_PATH, false).expect("open the lab car");
    let base_drive = final_drive(&panel);
    let (base_steps, _) = play_zero_to_100(&mut panel, false);
    let base_run = panel
        .run(ProvingScript::Accel, 20.0, Some("base"))
        .expect("base run")
        .run
        .zero_to_100_s
        .expect("base reaches 100");

    panel
        .set("drivetrain.gearbox.final_drive", json!(base_drive * 1.2), false)
        .expect("raise the final drive");
    let (tuned_steps, vehicles) = play_zero_to_100(&mut panel, true);
    let tuned_run = panel
        .run(ProvingScript::Accel, 20.0, Some("final drive +20 %"))
        .expect("tuned run")
        .run
        .zero_to_100_s
        .expect("tuned reaches 100");
    eprintln!(
        "play 0-100: {base_steps} -> {tuned_steps} steps; proving ground {base_run:.3} s -> {tuned_run:.3} s"
    );
    assert!(
        tuned_steps < base_steps,
        "a shorter gearing accelerates harder in play"
    );
    assert!(tuned_run < base_run, "and on the proving ground");

    // `physics.vehicles` reports the new config.
    let row = &vehicles["vehicles"][0];
    let edited = panel.asset.as_ref().expect("open").config.digest();
    assert_eq!(row["config_digest"], json!(format!("{edited:016x}")));
    assert_eq!(row["config_revision"], json!(1));
    assert_eq!(panel.runs.len(), 2);
    assert!(panel.telemetry.len() > 10, "the strip read the play car");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_value_is_refused_with_its_validate_code_and_changes_nothing() {
    let dir = project_with_lab_car("vehicle-tuning-refuse");
    let mut world = world_with_project(&dir);
    tuning_open(&mut world, &json!({ "path": CAR_PATH })).expect("open");
    let before = world.resource::<VehicleTuning>().asset.clone();
    for (field, value, code) in [
        (
            "drivetrain.gearbox.final_drive",
            json!(-1.0),
            "vehicle_invalid_config",
        ),
        ("chassis_mass", json!(0.0), "vehicle_invalid_config"),
        ("wheels.2.radius", json!(-0.3), "vehicle_invalid_wheel"),
        ("wheels", json!([]), "vehicle_no_wheels"),
        ("name", json!("  "), "vehicle_asset_name"),
        (
            "drivetrain.gearbox.nonsense",
            json!(1.0),
            "vehicle_tuning_bad_field",
        ),
        ("wheels.9.radius", json!(0.3), "vehicle_tuning_bad_field"),
        ("chassis_mass", json!("heavy"), "vehicle_tuning_bad_value"),
    ] {
        let e = tuning_set(&mut world, &json!({ "field": field, "value": value })).expect_err(field);
        assert_eq!(reason(&e).as_deref(), Some(code), "{field}");
        let panel = world.resource::<VehicleTuning>();
        assert_eq!(panel.asset, before, "{field} changed nothing");
        assert!(panel.undo.is_empty());
        assert_eq!(panel.error.as_ref().map(|e| e.code.as_str()), Some(code));
    }
    // The same codes as `physics.vehicle_config_check` gives for the same file.
    let mut bad = before.clone().expect("asset");
    bad.config.chassis_mass = 0.0;
    let text = bad.to_ron().expect("ron");
    let check = VehicleAsset::from_ron(&text).expect_err("refused");
    assert_eq!(check.code(), "vehicle_invalid_config");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_then_reload_equals_the_edited_asset_and_keeps_the_comments() {
    let dir = project_with_lab_car("vehicle-tuning-save");
    let mut panel = VehicleTuning::default();
    panel.open(Some(&dir), CAR_PATH, false).expect("open");
    let drive = final_drive(&panel);
    panel
        .set("drivetrain.gearbox.final_drive", json!(drive * 1.2), false)
        .expect("final drive");
    panel.set("chassis_mass", json!(1350.0), false).expect("mass");
    panel
        .set("wheels.0.suspension_stiffness", json!(34.0), false)
        .expect("spring");
    assert!(panel.is_dirty());
    let written = panel.save(Some(&dir), None).expect("save");
    assert_eq!(written, CAR_PATH);
    let text = std::fs::read_to_string(dir.join(CAR_PATH)).expect("read back");
    let reloaded = VehicleAsset::from_ron(&text).expect("reload");
    assert_eq!(
        Some(&reloaded),
        panel.asset.as_ref(),
        "save then reload equals the edit"
    );
    assert_eq!(
        regia_core::version::peek_ron_version(&text).expect("version"),
        regia::physics::vehicle_asset::VEHICLE_FORMAT_VERSION,
        "written as v4"
    );
    assert_eq!(panel.comments_kept, Some(true), "{text}");
    assert!(text.contains("// The lab's car"), "the comments stay:\n{text}");
    assert!(!panel.is_dirty());
    // A second save of an unchanged asset writes the same text.
    panel.save(Some(&dir), None).expect("again");
    assert_eq!(std::fs::read_to_string(dir.join(CAR_PATH)).expect("read"), text);
    // Save as: a new file in the canonical form.
    panel
        .save(Some(&dir), Some("assets/copy.vehicle.ron"))
        .expect("save as");
    let copy = std::fs::read_to_string(dir.join("assets/copy.vehicle.ron")).expect("copy");
    assert_eq!(
        VehicleAsset::from_ron(&copy).as_ref(),
        Ok(panel.asset.as_ref().expect("asset"))
    );
    // No temp file is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.join("assets"))
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_text_the_merge_cannot_reproduce_falls_back_to_the_canonical_form() {
    let asset = VehicleAsset::from_ron(LAB_CAR).expect("lab");
    let mut edited = asset.clone();
    edited.config.chassis_mass = 1500.0;
    // A source that is not the opened asset: the merge result loads differently, so canonical.
    let (text, kept) = save_text(
        Some("(format_version: 4, name: \"x\", config: ())"),
        Some(&asset),
        &edited,
    )
    .expect("text");
    assert!(!kept);
    assert_eq!(VehicleAsset::from_ron(&text), Ok(edited.clone()));
    let (text, kept) = save_text(None, None, &edited).expect("canonical");
    assert!(!kept);
    assert_eq!(text, edited.to_ron().expect("ron"));
}

#[test]
fn one_undo_step_per_edit_and_per_drag() {
    let dir = project_with_lab_car("vehicle-tuning-undo");
    let mut panel = VehicleTuning::default();
    panel.open(Some(&dir), CAR_PATH, false).expect("open");
    let original = panel.asset.clone();
    panel.set("chassis_mass", json!(1300.0), false).expect("edit 1");
    panel.set("chassis_mass", json!(1310.0), false).expect("edit 2");
    assert_eq!(panel.undo.len(), 2, "two edits, two steps");
    // A drag over many frames is one step.
    for frame in 0..20 {
        panel
            .set("wheels.0.radius", json!(0.35 + f64::from(frame) * 0.001), true)
            .expect("drag");
    }
    panel.end_drag();
    assert_eq!(panel.undo.len(), 3);
    // A second drag of the same field after the first ended is its own step.
    panel.set("wheels.0.radius", json!(0.30), true).expect("drag 2");
    panel.end_drag();
    assert_eq!(panel.undo.len(), 4);
    assert!(panel.undo());
    assert!(panel.undo());
    let radius = |p: &VehicleTuning| p.asset.as_ref().map(|a| a.config.wheels[0].radius);
    assert_eq!(
        radius(&panel),
        original.as_ref().map(|a| a.config.wheels[0].radius)
    );
    assert!(panel.redo());
    assert!((radius(&panel).expect("radius") - 0.369).abs() < 1e-6);
    assert!(panel.undo() && panel.undo() && panel.undo());
    assert_eq!(panel.asset, original);
    assert!(!panel.undo(), "nothing more to undo");
    assert_eq!(panel.asset, original);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sections_and_variants_are_editable_by_path() {
    let mut panel = VehicleTuning::default();
    let dir = regia_test_utils::unique_temp_dir("vehicle-tuning-create");
    panel
        .open(Some(&dir), "assets/new_car.vehicle.ron", true)
        .expect("create");
    assert_eq!(panel.asset.as_ref().map(|a| a.name.as_str()), Some("new_car"));
    panel
        .set("drivetrain", json!({ "Geared": {} }), false)
        .expect("geared");
    panel
        .set(
            "drivetrain.engine.torque_curve",
            json!([[1000.0, 200.0], [6000.0, 320.0], [7000.0, 280.0]]),
            false,
        )
        .expect("curve");
    panel
        .set("drivetrain.engine.torque_curve.1.1", json!(340.0), false)
        .expect("point");
    panel
        .set(
            "drivetrain.rear_differential",
            json!({ "LimitedSlip": { "torque_bias": 2.0 } }),
            false,
        )
        .expect("lsd");
    panel
        .set("drivetrain.rear_differential.torque_bias", json!(3.0), false)
        .expect("bias");
    panel
        .set("wheels.1.tire_model", json!({ "Slip": {} }), false)
        .expect("slip");
    panel
        .set("wheels.1.tire_model.cornering_stiffness", json!(55000.0), false)
        .expect("stiffness");
    panel.set("aero", json!({}), false).expect("aero on");
    panel
        .set("aero.drag_coefficient", json!(0.29), false)
        .expect("cd");
    let config = &panel.asset.as_ref().expect("asset").config;
    let regia::physics::vehicle_drivetrain::Drivetrain::Geared(d) = &config.drivetrain else {
        panic!("geared");
    };
    assert_eq!(d.engine.torque_curve[1], (6000.0, 340.0));
    assert_eq!(
        d.rear_differential,
        regia::physics::vehicle_drivetrain::Differential::LimitedSlip { torque_bias: 3.0 }
    );
    assert_eq!(
        config.wheels[1].tire_model.slip().map(|t| t.cornering_stiffness),
        Some(55000.0)
    );
    assert_eq!(config.aero.as_ref().map(|a| a.drag_coefficient), Some(0.29));
    // A slip field on a damper tire does not exist.
    assert_eq!(
        panel
            .set("wheels.0.tire_model.cornering_stiffness", json!(1.0), false)
            .map_err(|e| e.code),
        Err("vehicle_tuning_bad_field".to_string())
    );
    let state = panel.json(true);
    assert_eq!(state["plots"]["wheel_force"].as_array().map(Vec::len), Some(5));
    assert!(state["plots"]["torque_power"].as_array().is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_method_answers_by_its_registered_name() {
    let list = methods();
    let names: Vec<&str> = list.iter().map(|(m, _)| m.name).collect();
    assert_eq!(
        names,
        [
            "editor.vehicle_tuning_open",
            "editor.vehicle_tuning_state",
            "editor.vehicle_tuning_set",
            "editor.vehicle_tuning_undo",
            "editor.vehicle_tuning_redo",
            "editor.vehicle_tuning_apply",
            "editor.vehicle_tuning_save",
            "editor.vehicle_tuning_run",
            "editor.vehicle_tuning_fly",
            "editor.vehicle_tuning_ride",
        ]
    );
    let by_name = |name: &str| {
        list.iter()
            .find(|(m, _)| m.name == name)
            .map(|(_, h)| *h)
            .expect("registered")
    };
    let dir = project_with_lab_car("vehicle-tuning-names");
    let mut world = world_with_project(&dir);
    let e =
        by_name("editor.vehicle_tuning_set")(&mut world, &json!({ "field": "chassis_mass", "value": 1.0 }))
            .expect_err("nothing open");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_nothing_open"));
    let v = by_name("editor.vehicle_tuning_open")(&mut world, &json!({ "path": CAR_PATH })).expect("open");
    assert_eq!(v["name"], "vehicle lab rwd");
    by_name("editor.vehicle_tuning_set")(&mut world, &json!({ "field": "chassis_mass", "value": 1250.0 }))
        .expect("set");
    let v = by_name("editor.vehicle_tuning_state")(&mut world, &json!({ "plots": true })).expect("state");
    assert_eq!(v["dirty"], true);
    assert_eq!(v["undo_depth"], 1);
    assert!(v["plots"]["suspension"]["points"].is_array());
    by_name("editor.vehicle_tuning_undo")(&mut world, &json!({})).expect("undo");
    let v = by_name("editor.vehicle_tuning_redo")(&mut world, &json!({})).expect("redo");
    assert_eq!(v["config"]["chassis_mass"], 1250.0);
    let v = by_name("editor.vehicle_tuning_run")(&mut world, &json!({ "script": "skidpad", "seconds": 8.0 }))
        .expect("run");
    assert!(v["max_lateral_g"].as_f64().is_some_and(|g| g > 0.1), "{v}");
    let e = by_name("editor.vehicle_tuning_run")(&mut world, &json!({ "script": "drift" })).expect_err("bad");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_script"));
    let e = by_name("editor.vehicle_tuning_run")(&mut world, &json!({ "seconds": 500.0 })).expect_err("bad");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_seconds"));
    let e = by_name("editor.vehicle_tuning_apply")(&mut world, &json!({})).expect_err("not playing");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_not_playing"));
    let v = by_name("editor.vehicle_tuning_save")(&mut world, &json!({})).expect("save");
    assert_eq!(v["written"], CAR_PATH);
    for path in ["../out.vehicle.ron", "/etc/x.vehicle.ron", "assets/car.ron"] {
        let e = by_name("editor.vehicle_tuning_save")(&mut world, &json!({ "path": path })).expect_err(path);
        assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_path"), "{path}");
    }
    let e = by_name("editor.vehicle_tuning_open")(&mut world, &json!({ "path": "assets/none.vehicle.ron" }))
        .expect_err("missing");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_not_found"));
    let mut empty = World::new();
    let e = by_name("editor.vehicle_tuning_open")(&mut empty, &json!({ "path": CAR_PATH }))
        .expect_err("no project");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_no_project"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_without_a_matching_vehicle_names_the_problem() {
    let dir = project_with_lab_car("vehicle-tuning-target");
    let mut panel = VehicleTuning::default();
    panel.open(Some(&dir), CAR_PATH, false).expect("open");
    // Two vehicles, neither running this file: name one.
    let (mut app, first) = play_app(VehicleConfig::default());
    let second = VehicleConfig {
        chassis_mass: 900.0,
        ..VehicleConfig::default()
    };
    spawn_vehicle(app.world_mut(), second, Vec3::new(10.0, 0.9, 0.0), Quat::IDENTITY).expect("second");
    panel.pending_apply = Some(None);
    pump_world(&mut panel, Some(app.world_mut()));
    let refused = panel.last_apply.clone().expect("ran").expect_err("ambiguous");
    assert_eq!(refused.code, "vehicle_tuning_no_target");
    panel.pending_apply = Some(Some(first.to_bits()));
    pump_world(&mut panel, Some(app.world_mut()));
    assert!(panel.last_apply.clone().expect("ran").is_ok());
    // Without a play session the queued Apply fails.
    panel.pending_apply = Some(None);
    pump_world(&mut panel, None);
    let refused = panel.last_apply.clone().expect("ran").expect_err("not playing");
    assert_eq!(refused.code, "vehicle_tuning_not_playing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs the panel for `frames` frames on a headless egui context.
fn draw_frames(world: &mut World, frames: usize) -> usize {
    let ctx = egui::Context::default();
    let mut count = 0;
    for _ in 0..frames {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(520.0, 900.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            crate::vehicle_tuning_panel::vehicle_tuning_panel(ui, world);
        });
        output.textures_delta.clear();
        count = ctx.tessellate(output.shapes, output.pixels_per_point).len();
    }
    count
}

#[test]
fn the_panel_draws_headless_in_every_state() {
    let dir = project_with_lab_car("vehicle-tuning-draw");
    let mut world = world_with_project(&dir);
    assert!(draw_frames(&mut world, 2) > 0, "nothing open");
    tuning_open(&mut world, &json!({ "path": CAR_PATH })).expect("open");
    assert!(draw_frames(&mut world, 2) > 0, "open");
    tuning_set(
        &mut world,
        &json!({ "field": "wheels.0.tire_model", "value": { "Slip": {} } }),
    )
    .expect("slip");
    let _ = tuning_set(&mut world, &json!({ "field": "chassis_mass", "value": -1.0 }));
    tuning_run(&mut world, &json!({ "script": "accel", "seconds": 3.0 })).expect("run");
    assert!(draw_frames(&mut world, 2) > 0, "refusal, slip tire, a run");
    // Drawing changes nothing by itself.
    assert_eq!(world.resource::<VehicleTuning>().undo.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_refuses_an_existing_file_and_opens_nothing() {
    let dir = project_with_lab_car("vehicle-tuning-create-exists");
    let mut panel = VehicleTuning::default();
    let e = panel
        .open(Some(&dir), CAR_PATH, true)
        .expect_err("the file exists");
    assert_eq!(e.code, "vehicle_tuning_exists");
    assert!(panel.asset.is_none(), "an refused create opens nothing");
    // The agent method reports the same code.
    let mut world = world_with_project(&dir);
    let e =
        tuning_open(&mut world, &json!({ "path": CAR_PATH, "create": true })).expect_err("the file exists");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_exists"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn opening_another_car_drops_the_apply_queue_target_runs_and_telemetry_entity() {
    let dir = project_with_lab_car("vehicle-tuning-open-resets");
    std::fs::write(dir.join("assets/other.vehicle.ron"), LAB_CAR).expect("second car file");
    let mut panel = VehicleTuning::default();
    panel.open(Some(&dir), CAR_PATH, false).expect("open the lab car");
    panel
        .run(ProvingScript::Accel, 5.0, Some("base"))
        .expect("a recorded run");
    panel.pending_apply = Some(Some(42));
    panel.target = Some(42);
    panel.telemetry_entity = Some(42);
    assert_eq!(panel.runs.len(), 1, "a run was recorded");
    // Open a different car: the queued Apply, the explicit target, the runs of the previous car
    // and the stale live-telemetry entity must not leak into it (review fixrev8 finding 2).
    panel
        .open(Some(&dir), "assets/other.vehicle.ron", false)
        .expect("open the other car");
    assert!(panel.pending_apply.is_none(), "the queued Apply was dropped");
    assert!(panel.target.is_none(), "the explicit target was dropped");
    assert!(panel.runs.is_empty(), "the previous car's runs were dropped");
    assert!(
        panel.telemetry_entity.is_none(),
        "the telemetry entity was dropped"
    );
    // Opening the same car again drops the state too.
    panel.pending_apply = Some(None);
    panel
        .open(Some(&dir), CAR_PATH, false)
        .expect("reopen the lab car");
    assert!(panel.pending_apply.is_none());
    assert!(panel.runs.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
