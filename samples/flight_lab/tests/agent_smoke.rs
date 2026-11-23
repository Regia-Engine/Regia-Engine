// SPDX-License-Identifier: MIT-0
//! An agent flies the lab (rule 8, ADR-3374): headless with no renderer, a client thread checks the
//! plane's file with `physics.aircraft_config_check`, releases the brakes and opens the throttle with
//! `physics.aircraft_input`, rotates and climbs while reading `physics.aircraft`, then levels off at
//! 150 m with `physics.aircraft_trim` as the pitch and throttle feed-forward. No golden image.
//!
//! Each frame advances the clock by one 60 Hz fixed step, and the run is paused with explicit
//! `play.step`s, so it does not depend on the machine's speed.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use flight_lab::{FlightLabPlugin, PLANE_TEXT};
use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};

/// A hang guard only; nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// Frames per `play.step` while the agent flies (one fixed step each).
const CHUNK: u32 = 6;
/// The altitude the agent levels off at, meters.
const LEVEL: f64 = 150.0;

fn app(project: PathBuf) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, FlightLabPlugin))
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project),
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
    app
}

/// One agent call. The protocol allows 50 mutating calls per second (`F0007`), and this script can
/// outrun that, so it waits and retries; nothing asserts on time.
fn call(client: &mut Client, method: &str, params: Value) -> Value {
    for _ in 0..100 {
        match client.call(method, params.clone()) {
            Ok(value) => return value,
            Err(err) if format!("{err:?}").contains("F0007") => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => panic!("{method} failed: {err:?}"),
        }
    }
    panic!("{method}: still rate limited after 100 tries");
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

fn plane(client: &mut Client, entity: u64) -> Value {
    call(client, "physics.aircraft", json!({ "entity": entity }))["aircraft"][0].clone()
}

/// Pitch input that holds `target` degrees of pitch attitude around a feed-forward `trim`, damped by
/// the pitch rate (rad/s).
fn hold_pitch(row: &Value, target: f64, trim: f64) -> f64 {
    let pitch = number(&row["pitch_deg"]).to_radians();
    let rate = number(&row["body_rates"][0]);
    (trim + 3.0 * (target.to_radians() - pitch) - rate).clamp(-1.0, 1.0)
}

fn fly(client: &mut Client) {
    call(
        client,
        "agent.hello",
        json!({ "client": "flight-lab", "model": "worker" }),
    );
    let mut listed = Value::Null;
    for _ in 0..600 {
        listed = call(client, "physics.aircraft", json!({}));
        if listed["count"] == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(listed["count"], 1, "the lab has one plane: {listed}");
    let entity = listed["aircraft"][0]["entity"].as_u64().expect("plane id");
    call(client, "play.pause", json!({}));

    let check = call(
        client,
        "physics.aircraft_config_check",
        json!({ "ron": PLANE_TEXT }),
    );
    assert_eq!(check["ok"], true, "{check}");
    let stall_speed = number(&check["stall_speed"]);
    assert!(stall_speed > 15.0 && stall_speed < 35.0, "{check}");

    // Parked: the brakes are set and every wheel is on the runway.
    call(client, "play.step", json!({ "frames": 60 }));
    let parked = plane(client, entity);
    assert_eq!(parked["on_ground"], true, "{parked}");
    assert_eq!(parked["grounded_gear"], 3, "{parked}");
    let start_z = number(&parked["position"][2]);

    // Take-off roll: full throttle, brakes off; rotate at 1.1 × the stall speed.
    call(
        client,
        "physics.aircraft_input",
        json!({ "entity": entity, "throttle": 1.0, "brakes": 0.0 }),
    );
    let mut lift_off = None;
    let mut row = parked;
    for _ in 0..2000 {
        call(client, "play.step", json!({ "frames": CHUNK }));
        row = plane(client, entity);
        let airspeed = number(&row["airspeed"]);
        let altitude = number(&row["altitude"]);
        if lift_off.is_none() && row["on_ground"] == false && altitude > 3.0 {
            lift_off = Some(start_z - number(&row["position"][2]));
        }
        if altitude > LEVEL - 25.0 {
            break;
        }
        if airspeed > 1.1 * stall_speed {
            let roll = (-1.5 * number(&row["bank_deg"]).to_radians()).clamp(-1.0, 1.0);
            let pitch = hold_pitch(&row, 8.0, 0.25);
            call(
                client,
                "physics.aircraft_input",
                json!({ "entity": entity, "pitch": pitch, "roll": roll }),
            );
        }
        assert_eq!(row["stalled"], false, "stalled in the climb: {row}");
    }
    let lift_off = lift_off.expect("the plane left the runway");
    assert!((120.0..=500.0).contains(&lift_off), "lift-off after {lift_off} m");
    assert!(number(&row["altitude"]) > LEVEL - 30.0, "never climbed: {row}");

    // Level off: trim at the current speed and 150 m, then hold the altitude around the trim.
    let speed = number(&row["airspeed"]).clamp(35.0, 60.0);
    let trim = call(
        client,
        "physics.aircraft_trim",
        json!({ "entity": entity, "speed": speed, "altitude": LEVEL }),
    );
    let trim_pitch = number(&trim["pitch_input"]);
    let trim_deg = number(&trim["pitch_deg"]);
    call(
        client,
        "physics.aircraft_input",
        json!({ "entity": entity, "throttle": number(&trim["throttle"]), "pitch": trim_pitch }),
    );
    for _ in 0..300 {
        call(client, "play.step", json!({ "frames": CHUNK }));
        row = plane(client, entity);
        let altitude = number(&row["altitude"]);
        let climb = number(&row["linear_velocity"][1]);
        let wanted_climb = (0.15 * (LEVEL - altitude)).clamp(-3.0, 3.0);
        let target = trim_deg + 2.0 * (wanted_climb - climb);
        let pitch = hold_pitch(&row, target.clamp(-6.0, 10.0), trim_pitch);
        let roll = (-1.5 * number(&row["bank_deg"]).to_radians()).clamp(-1.0, 1.0);
        call(
            client,
            "physics.aircraft_input",
            json!({ "entity": entity, "pitch": pitch, "roll": roll }),
        );
        assert_eq!(row["stalled"], false, "stalled while leveling off: {row}");
    }
    let altitude = number(&row["altitude"]);
    let climb = number(&row["linear_velocity"][1]);
    eprintln!("lift-off after {lift_off:.0} m; level at {altitude:.1} m, climb {climb:.2} m/s; trim {trim}");
    assert!((altitude - LEVEL).abs() < 25.0, "not level: {row}");
    assert!(climb.abs() < 2.0, "still climbing or sinking: {row}");
    assert_eq!(row["error"], Value::Null);
}

#[test]
fn an_agent_takes_off_climbs_and_levels_off() {
    let project = regia_test_utils::unique_temp_dir("regia-flight-lab");
    let _ = std::fs::create_dir_all(&project);
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let token_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("flight-lab".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(token) =
                        std::fs::read_to_string(endpoint::agent_dir(&token_project).join("token"))
                    && let Ok(connected) = Client::connect(&advertised.url, &token)
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            fly(&mut client);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = app(project.clone());
    app.finish_plugins();
    while !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    let joined = script.join();
    let _ = std::fs::remove_dir_all(&project);
    assert!(joined.is_ok(), "the agent script panicked (see above)");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the agent script did not finish in time"
    );
}
