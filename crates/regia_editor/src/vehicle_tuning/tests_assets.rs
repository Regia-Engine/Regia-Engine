// SPDX-License-Identifier: MPL-2.0
//! Trailer and aircraft tuning, the proving-ground tracks and towing (ADR-7745). Fixed steps, no
//! clock.

use std::path::Path;

use regia::physics::aero::asset::AircraftAsset;
use regia::physics::vehicle_trailer::TrailerConfig;
use regia::physics::vehicle_trailer_asset::TrailerAsset;
use serde_json::{Value, json};

use super::*;

const LAB_CAR: &str = include_str!("../../tests/fixtures/vehicle_tuning/rwd_car.vehicle.ron");
const CAR_PATH: &str = "assets/rwd_car.vehicle.ron";
const TRAILER_PATH: &str = "assets/box.trailer.ron";
const PLANE_PATH: &str = "assets/trainer.aircraft.ron";

fn project(tag: &str) -> std::path::PathBuf {
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

fn call(world: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
    let list = methods();
    let handler = list
        .iter()
        .find(|(m, _)| m.name == name)
        .map(|(_, h)| *h)
        .unwrap_or_else(|| panic!("{name} is not registered"));
    handler(world, &params)
}

/// Acceptance (item 1): a created `.trailer.ron`, edited and saved through the agent methods, is
/// the canonical writer's bytes; undo and redo step the trailer, not the car.
#[test]
fn trailer_save_writes_the_canonical_writers_bytes() {
    let dir = project("vt-trailer-bytes");
    let mut world = world_with_project(&dir);
    let v = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": TRAILER_PATH, "create": true }),
    )
    .expect("create");
    assert_eq!(v["opened"], "trailer");
    assert_eq!(v["trailer"]["name"], "box");
    assert_eq!(v["open"], false, "the car stays closed");
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "trailer", "field": "chassis.chassis_mass", "value": 900.0 }),
    )
    .expect("mass");
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "trailer", "field": "coupling.point.2", "value": -2.5 }),
    )
    .expect("coupling point");
    let v = call(
        &mut world,
        "editor.vehicle_tuning_undo",
        json!({ "asset": "trailer" }),
    )
    .expect("undo");
    assert_eq!(v["changed"], true);
    assert_eq!(v["trailer"]["undo_depth"], 1);
    call(
        &mut world,
        "editor.vehicle_tuning_redo",
        json!({ "asset": "trailer" }),
    )
    .expect("redo");
    let v = call(
        &mut world,
        "editor.vehicle_tuning_save",
        json!({ "asset": "trailer" }),
    )
    .expect("save");
    assert_eq!(v["written"], TRAILER_PATH);
    assert_eq!(v["asset"], "trailer");
    let mut expected = TrailerAsset::new("box", TrailerConfig::default());
    expected.config.chassis.chassis_mass = 900.0;
    expected.config.coupling.point.z = -2.5;
    let bytes = std::fs::read(dir.join(TRAILER_PATH)).expect("written");
    assert_eq!(
        bytes,
        expected.to_ron().expect("canonical").into_bytes(),
        "the saved bytes are TrailerAsset::to_ron's"
    );
    // Reopened, it is the edited trailer and clean.
    let v = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": TRAILER_PATH }),
    )
    .expect("reopen");
    assert_eq!(v["trailer"]["dirty"], false);
    assert_eq!(v["trailer"]["config"]["chassis"]["chassis_mass"], 900.0);
    // A refused value names the trailer's own code and changes nothing.
    let e = call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "trailer", "field": "parking_brake", "value": 2.0 }),
    )
    .expect_err("refused");
    assert_eq!(reason(&e).as_deref(), Some("trailer_invalid_config"));
    let v = call(&mut world, "editor.vehicle_tuning_state", json!({})).expect("state");
    assert_eq!(v["trailer"]["undo_depth"], 0);
    assert_eq!(v["trailer"]["error"]["code"], "trailer_invalid_config");
    // The wrong ending for the document, and an unknown document.
    let e = call(
        &mut world,
        "editor.vehicle_tuning_save",
        json!({ "asset": "trailer", "path": "assets/x.vehicle.ron" }),
    )
    .expect_err("ending");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_path"));
    let e = call(
        &mut world,
        "editor.vehicle_tuning_undo",
        json!({ "asset": "boat" }),
    )
    .expect_err("kind");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_asset"));
    let e = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": "assets/x.boat.ron" }),
    )
    .expect_err("kind");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_path"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hand-written trailer keeps its comments through an edit and a save.
#[test]
fn trailer_save_keeps_the_files_comments() {
    let dir = project("vt-trailer-comments");
    let text = format!(
        "// the club's box trailer\n{}",
        TrailerAsset::new("club", TrailerConfig::default())
            .to_ron()
            .expect("ron")
    );
    std::fs::write(dir.join(TRAILER_PATH), &text).expect("write");
    let mut panel = VehicleTuning::default();
    panel.open_any(Some(&dir), TRAILER_PATH, false).expect("open");
    panel
        .set_in(AssetKind::Trailer, "parking_brake", json!(0.5), false)
        .expect("set");
    panel.save_in(AssetKind::Trailer, Some(&dir), None).expect("save");
    assert_eq!(panel.others.trailer.comments_kept, Some(true));
    let saved = std::fs::read_to_string(dir.join(TRAILER_PATH)).expect("read");
    assert!(saved.starts_with("// the club's box trailer"), "{saved}");
    let loaded = TrailerAsset::from_ron(&saved).expect("loads");
    assert_eq!(loaded.config.parking_brake, 0.5);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance (item 1): the proving ground hitches the open trailer to the tuned car, and a run
/// reads the trailer's edited mass (counted: two runs, fixed steps).
#[test]
fn a_towing_run_reads_the_edited_trailer() {
    let dir = project("vt-trailer-tow");
    let mut world = world_with_project(&dir);
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": CAR_PATH }),
    )
    .expect("car");
    let e = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 5.0, "tow": true }),
    )
    .expect_err("no trailer");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_no_trailer"));
    // The refused run left `tow` off (the GUI's disabled checkbox could not clear it).
    let v = call(&mut world, "editor.vehicle_tuning_state", json!({})).expect("state");
    assert_eq!(v["tow"], false, "a refused run keeps the panel's tow");
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": TRAILER_PATH, "create": true }),
    )
    .expect("trailer");
    let solo = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 5.0, "tow": false }),
    )
    .expect("solo");
    assert!(solo["trailer"].is_null());
    let light = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 5.0, "tow": true }),
    )
    .expect("light");
    assert_eq!(light["trailer"]["chassis_mass"], 750.0);
    assert_eq!(light["trailer"]["coupled"], true);
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "trailer", "field": "chassis.chassis_mass", "value": 1500.0 }),
    )
    .expect("heavier");
    let heavy = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 5.0 }),
    )
    .expect("heavy (tow is kept)");
    assert_eq!(heavy["trailer"]["chassis_mass"], 1500.0);
    let distance = |v: &Value| v["distance_m"].as_f64().expect("distance");
    assert!(distance(&light) < distance(&solo), "{light} vs {solo}");
    assert!(distance(&heavy) < distance(&light), "{heavy} vs {light}");
    assert_ne!(
        light["trailer"]["config_digest"],
        heavy["trailer"]["config_digest"]
    );
    let v = call(&mut world, "editor.vehicle_tuning_state", json!({})).expect("state");
    assert_eq!(v["tow"], true);
    assert_eq!(v["runs"].as_array().map(Vec::len), Some(3));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Acceptance (item 3) through the panel's model: a bump-strip run changes the car's wheel
/// contacts, a flat one does not; an unknown track is refused.
#[test]
fn the_bump_strip_run_changes_the_contact_count() {
    let dir = project("vt-tracks");
    let mut world = world_with_project(&dir);
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": CAR_PATH }),
    )
    .expect("car");
    let flat = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 10.0, "track": "flat" }),
    )
    .expect("flat");
    assert_eq!(flat["track"], "flat");
    assert_eq!(flat["contact_changes"], 0, "{flat}");
    let bumps = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 10.0, "track": "bump_strip" }),
    )
    .expect("bumps");
    assert_eq!(bumps["track"], "bump_strip");
    assert!(
        bumps["contact_changes"].as_u64().is_some_and(|n| n > 0),
        "{bumps}"
    );
    assert!(
        bumps["min_contacts"].as_u64() < bumps["max_contacts"].as_u64(),
        "{bumps}"
    );
    let ramp = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "script": "accel", "seconds": 6.0, "track": "ramp" }),
    )
    .expect("ramp");
    assert!(ramp["max_rise_m"].as_f64().is_some_and(|r| r > 1.0), "{ramp}");
    let e = call(
        &mut world,
        "editor.vehicle_tuning_run",
        json!({ "track": "moon" }),
    )
    .expect_err("track");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_track"));
    let v = call(&mut world, "editor.vehicle_tuning_state", json!({})).expect("state");
    assert_eq!(v["track"], "ramp", "the track is kept for later runs");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 2: the aircraft's parameters as forms; a flight reads the edited value; the save is the
/// canonical writer's bytes.
#[test]
fn an_aircraft_edit_is_read_by_the_next_flight() {
    let dir = project("vt-aircraft");
    let mut world = world_with_project(&dir);
    let e = call(&mut world, "editor.vehicle_tuning_fly", json!({})).expect_err("nothing open");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_nothing_open"));
    let v = call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": PLANE_PATH, "create": true }),
    )
    .expect("create");
    assert_eq!(v["opened"], "aircraft");
    assert_eq!(v["aircraft"]["config"]["mass"], 1000.0);
    let first = call(&mut world, "editor.vehicle_tuning_fly", json!({ "seconds": 5.0 })).expect("fly");
    assert_eq!(first["mass"], 1000.0);
    assert!(
        first["max_altitude_change_m"].as_f64().is_some_and(|dy| dy < 2.0),
        "{first}"
    );
    call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "aircraft", "field": "mass", "value": 1200.0 }),
    )
    .expect("mass");
    let second = call(&mut world, "editor.vehicle_tuning_fly", json!({ "seconds": 5.0 })).expect("fly");
    assert_eq!(second["mass"], 1200.0);
    assert!(
        second["alpha_deg"].as_f64() > first["alpha_deg"].as_f64(),
        "a heavier plane trims at a higher angle of attack: {first} {second}"
    );
    assert_ne!(first["digest"], second["digest"]);
    let again = call(&mut world, "editor.vehicle_tuning_fly", json!({ "seconds": 5.0 })).expect("fly");
    assert_eq!(
        again["digest"], second["digest"],
        "fixed steps: the same flight twice"
    );
    let e = call(
        &mut world,
        "editor.vehicle_tuning_set",
        json!({ "asset": "aircraft", "field": "mass", "value": -1.0 }),
    )
    .expect_err("refused");
    assert_eq!(reason(&e).as_deref(), Some("aircraft_invalid_config"));
    let e = call(&mut world, "editor.vehicle_tuning_fly", json!({ "seconds": 0.0 })).expect_err("seconds");
    assert_eq!(reason(&e).as_deref(), Some("vehicle_tuning_bad_seconds"));
    call(
        &mut world,
        "editor.vehicle_tuning_save",
        json!({ "asset": "aircraft" }),
    )
    .expect("save");
    let mut expected = AircraftAsset {
        name: "trainer".to_string(),
        ..AircraftAsset::default()
    };
    expected.config.mass = 1200.0;
    assert_eq!(
        std::fs::read(dir.join(PLANE_PATH)).expect("written"),
        expected.to_ron().expect("canonical").into_bytes()
    );
    let v = call(&mut world, "editor.vehicle_tuning_state", json!({})).expect("state");
    assert_eq!(v["flights"].as_array().map(Vec::len), Some(3));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_panel_draws_the_trailer_and_aircraft_sections_headless() {
    let dir = project("vt-assets-draw");
    let mut world = world_with_project(&dir);
    for path in [TRAILER_PATH, PLANE_PATH] {
        call(
            &mut world,
            "editor.vehicle_tuning_open",
            json!({ "path": path, "create": true }),
        )
        .expect(path);
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
    call(
        &mut world,
        "editor.vehicle_tuning_open",
        json!({ "path": CAR_PATH }),
    )
    .expect("car");
    call(&mut world, "editor.vehicle_tuning_fly", json!({ "seconds": 1.0 })).expect("fly");
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
    assert_eq!(panel.others.trailer.undo.len(), 0, "drawing changes nothing");
    assert_eq!(panel.others.aircraft.undo.len(), 0, "drawing changes nothing");
    let _ = std::fs::remove_dir_all(&dir);
}
