// SPDX-License-Identifier: MIT-0
//! The lab's physics without a renderer: the asset loads as v2, two runs give the same digests and
//! both vehicles move. No GPU, window or wall clock.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use regia::prelude::*;
use regia_agent::{client::Client, endpoint};
use regia_physics::vehicle::VehicleController;
use regia_physics::vehicle_asset::{VEHICLE_FORMAT_VERSION, VehicleAsset};
use regia_physics::vehicle_drivetrain::{DriveLayout, Drivetrain};
use serde_json::{Value, json};
use vehicle_lab::{CAR_SPAWN, LabEntities, LabScript, VehicleLabPlugin};

#[test]
fn the_car_asset_is_a_rear_wheel_drive_v2_car() {
    let asset = VehicleAsset::from_ron(vehicle_lab::CAR_TEXT).expect("loads");
    assert_eq!(asset.format_version, VEHICLE_FORMAT_VERSION);
    let Drivetrain::Geared(drivetrain) = &asset.config.drivetrain else {
        panic!("the lab's car is geared");
    };
    assert_eq!(drivetrain.layout, DriveLayout::Rwd);
    assert_eq!(asset.config.anti_roll.len(), 2);
    assert!(asset.config.aero.is_some());
    assert!(asset.config.aids.abs.is_some() && asset.config.aids.traction_control.is_some());
}

#[test]
fn the_slip_car_is_the_lab_car_on_slip_tires_with_no_tuning_warnings() {
    use regia_physics::vehicle_tire::{LateralCurve, tire_warnings};
    let slip = VehicleAsset::from_ron(vehicle_lab::SLIP_CAR_TEXT).expect("loads");
    let plain = VehicleAsset::from_ron(vehicle_lab::CAR_TEXT).expect("loads");
    assert_eq!(slip.format_version, VEHICLE_FORMAT_VERSION);
    assert_eq!(slip.config.drivetrain, plain.config.drivetrain);
    assert_eq!(slip.config.anti_roll, plain.config.anti_roll);
    assert_eq!(slip.config.aero, plain.config.aero);
    let tires: Vec<_> = slip
        .config
        .wheels
        .iter()
        .map(|wheel| {
            wheel
                .tire_model
                .slip()
                .expect("every wheel has a slip tire")
                .curve
        })
        .collect();
    assert_eq!(tires[0], LateralCurve::Brush);
    assert!(matches!(tires[3], LateralCurve::MagicFormula { .. }));
    assert_eq!(tire_warnings(&slip.config), Vec::new());
}

#[test]
fn two_headless_runs_are_bit_identical_and_the_car_shifts() {
    let first = vehicle_lab::run_headless().expect("first run");
    let second = vehicle_lab::run_headless().expect("second run");
    assert_eq!(first, second);
    assert!(first.upshifts >= 2, "{first:?}");
    assert!(first.car_speed > 20.0, "{first:?}");
    assert!(first.craft_speed > 10.0, "{first:?}");
}

#[test]
fn the_script_loops_on_the_track() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VehicleLabPlugin))
        .insert_resource(LabScript)
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
    for _ in 0..3 {
        app.update();
    }
    let car = app.world().resource::<LabEntities>().car;
    let (mut farthest, mut returns, mut last) = (0.0_f32, 0, CAR_SPAWN.z);
    // 65 s of the 20 s loop: left alone, the car would run off the 3 km track within a few minutes.
    for _ in 0..3900 {
        app.update();
        let world = app.world();
        let physics = world.resource::<regia_physics::PhysicsWorld>();
        let controller = world.get::<VehicleController>(car).expect("the car");
        let z = controller.vehicle.state(physics).expect("state").translation.z;
        farthest = farthest.max((z - CAR_SPAWN.z).abs());
        if z - last > 100.0 {
            returns += 1;
        }
        last = z;
    }
    assert_eq!(returns, 3, "one return per finished loop");
    assert!(farthest < 900.0, "{farthest}");
    assert!(farthest > 100.0, "the car really ran: {farthest}");
}

#[derive(Debug)]
struct EpisodeReport {
    project: PathBuf,
    endpoint_url: String,
    token: String,
    baseline_bodies: Value,
    baseline_vehicles: Value,
    drive_report: Value,
}

/// Runs one paused, fixed-step episode through the same agent methods a user drives.
fn run_episode(kind: EpisodeKind) -> EpisodeReport {
    let project = regia_test_utils::temp::unique_temp_dir("vehicle-lab-episode");
    std::fs::create_dir_all(&project).expect("create episode project");

    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VehicleLabPlugin, regia_agent::AgentPlugin))
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project.clone()),
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    // Headless `MinimalPlugins` has no platform runner. The resource and the message are what
    // `PlatformPlugin` installs; the shutdown below calls `apply_transition`, which needs both
    // (without them it panicked on the missing `AppLifecycle` on every host).
    app.init_resource::<regia_platform::AppLifecycle>()
        .add_message::<regia_platform::LifecycleEvent>();
    app.finish_plugins();
    // Match run_headless's startup and settling frames before beginning the scripted course.
    for _ in 0..63 {
        app.update();
    }
    // Freeze game time now, as the client's first `play.pause` does: the frame pump below runs
    // frames while the client connects and until its pause is served, and an unpaused frame
    // there steps physics. How many depended on thread timing, so two A courses ran 1267 and
    // 1265 steps with different digests (ADR-7386).
    app.world_mut().resource_mut::<Time>().paused = true;
    assert!(app.world().get_resource::<LabEntities>().is_some());
    assert!(app.world().get_resource::<LabScript>().is_none());
    let controllers = app
        .world_mut()
        .query::<&VehicleController>()
        .iter(app.world())
        .count();
    assert_eq!(controllers, 1, "one fresh car controller per episode");

    let agent_dir = endpoint::agent_dir(&project);
    let (advertised, token) = endpoint::read(&agent_dir).expect("fresh endpoint is advertised");
    let mut client = Client::connect(&advertised.url, &token).expect("connect to this episode");
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let script = std::thread::Builder::new()
        .name("vehicle-lab-episode-client".into())
        .spawn(move || {
            started_tx.send(()).expect("signal client start");
            let result = drive_episode(&mut client, kind);
            done_tx.send(result).expect("send episode result");
        })
        .expect("spawn episode client");
    started_rx.recv().expect("client starts before pumping frames");

    // The protocol call replies only after the corresponding app frames have run. The bounded
    // frame pump is released by the client's completion channel; no time-based polling is used.
    let mut result = None;
    for _ in 0..12_000 {
        app.update();
        if let Ok(done) = done_rx.try_recv() {
            result = Some(done);
            break;
        }
        std::thread::yield_now();
    }
    let client_result = result.expect("episode finished within its fixed frame bound");
    script.join().expect("client thread did not panic");
    let (baseline_bodies, baseline_vehicles, drive_report) = client_result.expect("agent episode completed");

    // Exercise the app's normal shutdown hook and check its own endpoint is removed before the
    // per-episode project is deleted.
    regia_platform::apply_transition(&mut app, regia_platform::LifecycleEvent::WillTerminate);
    assert!(
        endpoint::read(&agent_dir).is_err(),
        "terminated endpoint is stale"
    );
    drop(app);
    std::fs::remove_dir_all(&project).expect("remove episode project and token");
    assert!(!project.exists(), "episode temporary project is gone");

    EpisodeReport {
        project,
        endpoint_url: advertised.url,
        token,
        baseline_bodies,
        baseline_vehicles,
        drive_report,
    }
}

#[derive(Clone, Copy)]
enum EpisodeKind {
    CourseA,
    DisturbanceB,
}

fn agent_call(client: &mut Client, method: &str, params: Value) -> Value {
    client
        .call(method, params)
        .unwrap_or_else(|error| panic!("{method} failed: {error:?}"))
}

fn drive_episode(client: &mut Client, kind: EpisodeKind) -> Result<(Value, Value, Value), String> {
    agent_call(
        client,
        "agent.hello",
        json!({ "client": "vehicle-lab-episode-isolation", "model": "test" }),
    );
    agent_call(client, "play.pause", json!({}));
    let vehicles = agent_call(client, "physics.vehicles", json!({}));
    if vehicles["count"] != 1 {
        return Err(format!("expected one car in a fresh episode: {vehicles}"));
    }
    let car = vehicles["vehicles"][0]["entity"]
        .as_u64()
        .ok_or_else(|| format!("car entity is absent: {vehicles}"))?;
    let hovercraft = agent_call(client, "physics.hovercraft", json!({}));
    if hovercraft["count"] != 1 {
        return Err(format!(
            "expected one hover craft in a fresh episode: {hovercraft}"
        ));
    }
    let craft = hovercraft["hovercraft"][0]["entity"]
        .as_u64()
        .ok_or_else(|| format!("hover craft entity is absent: {hovercraft}"))?;
    let baseline_bodies = agent_call(client, "physics.stats", json!({}));
    let baseline_input = &vehicles["vehicles"][0]["input"];
    if baseline_input != &json!({ "throttle": 0.0, "brake": 0.0, "steer": 0.0 }) {
        return Err(format!("fresh episode inherited input state: {baseline_input}"));
    }

    match kind {
        EpisodeKind::CourseA => {
            for entity in [car, craft] {
                agent_call(
                    client,
                    "physics.vehicle_input",
                    json!({ "entity": entity, "throttle": 1.0 }),
                );
            }
            agent_call(client, "play.step", json!({ "frames": 1200 }));
            let stats = agent_call(client, "physics.stats", json!({}));
            let telemetry = agent_call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
            let craft = agent_call(client, "physics.hovercraft", json!({ "entity": craft }));
            let car_row = &telemetry["vehicles"][0];
            let craft_row = &craft["hovercraft"][0];
            let report = json!({
                "digest": stats["digest"],
                "steps": stats["steps"],
                "upshifts": car_row["upshifts"],
                "gear": car_row["gear"],
                "rpm": car_row["rpm"],
                "speed": car_row["speed"],
                "craft_speed": craft_row["forward_speed"],
                "bodies": stats["bodies"],
                "colliders": stats["colliders"],
            });
            Ok((baseline_bodies, vehicles, report))
        }
        EpisodeKind::DisturbanceB => {
            agent_call(
                client,
                "physics.vehicle_input",
                json!({ "entity": car, "throttle": 0.6, "steer": 0.8 }),
            );
            agent_call(client, "play.step", json!({ "frames": 180 }));
            let turning = agent_call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
            let turning_speed = turning["vehicles"][0]["speed"]
                .as_f64()
                .ok_or_else(|| format!("turning speed missing: {turning}"))?;
            agent_call(
                client,
                "physics.vehicle_input",
                json!({ "entity": car, "throttle": 0.0, "brake": 1.0, "steer": -0.4 }),
            );
            agent_call(client, "play.step", json!({ "frames": 120 }));
            let final_vehicles = agent_call(client, "physics.vehicles", json!({ "entity": car }));
            let braking = agent_call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
            let final_speed = braking["vehicles"][0]["speed"]
                .as_f64()
                .ok_or_else(|| format!("braking speed missing: {braking}"))?;
            let input = &final_vehicles["vehicles"][0]["input"];
            // The controller stores f32: compare with -0.4 as an f32 reads back
            // (-0.4000000059604645), not the f64 literal (ADR-7386).
            if input != &json!({ "throttle": 0.0, "brake": 1.0, "steer": f64::from(-0.4f32) }) {
                return Err(format!("braking input was not applied: {input}"));
            }
            if turning_speed <= 1.0 || final_speed >= turning_speed {
                return Err(format!(
                    "steering/braking did not produce distinct motion: turning={turning_speed}, braking={final_speed}"
                ));
            }
            let report = json!({
                "turning_speed": turning_speed,
                "braking_speed": final_speed,
                "input": input,
                "turning_wheels": turning["vehicles"][0]["wheels"],
            });
            Ok((baseline_bodies, vehicles, report))
        }
    }
}

#[test]
fn agent_driven_a_b_a_episodes_are_isolated_in_one_process() {
    let first = run_episode(EpisodeKind::CourseA);
    let disturbance = run_episode(EpisodeKind::DisturbanceB);
    let last = run_episode(EpisodeKind::CourseA);

    assert_eq!(
        first.drive_report, last.drive_report,
        "A/B/A course reports agree"
    );
    assert_eq!(
        first.baseline_bodies["bodies"],
        disturbance.baseline_bodies["bodies"]
    );
    assert_eq!(
        first.baseline_bodies["colliders"],
        disturbance.baseline_bodies["colliders"]
    );
    assert_eq!(
        first.baseline_bodies["digest"], disturbance.baseline_bodies["digest"],
        "a fresh episode has the same baseline physics state"
    );
    assert_eq!(
        first.baseline_vehicles["count"],
        disturbance.baseline_vehicles["count"]
    );
    assert_eq!(disturbance.baseline_vehicles["count"], 1);
    assert!(
        disturbance.drive_report["turning_speed"]
            .as_f64()
            .expect("turning_speed")
            > 1.0
    );
    assert!(
        disturbance.drive_report["braking_speed"]
            .as_f64()
            .expect("braking_speed")
            < disturbance.drive_report["turning_speed"]
                .as_f64()
                .expect("turning_speed")
    );

    let projects = [&first.project, &disturbance.project, &last.project];
    assert!(projects[0] != projects[1] && projects[1] != projects[2]);
    assert!(first.endpoint_url.starts_with("ws://127.0.0.1:"));
    assert!(disturbance.endpoint_url.starts_with("ws://127.0.0.1:"));
    assert!(last.endpoint_url.starts_with("ws://127.0.0.1:"));
    assert!(first.token != disturbance.token && disturbance.token != last.token);
    assert!(projects.iter().all(|project| !project.exists()));
}
