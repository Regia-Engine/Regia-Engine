// SPDX-License-Identifier: MIT-0
//! An agent drives the lab (rule 8, ADR-2598): headless with the renderer on (lavapipe in CI), a
//! scripted throttle and steer for a fixed number of steps through `physics.vehicle_input`, gears
//! read through `physics.vehicle_telemetry`, a manual shift through `physics.vehicle_shift`, the hover
//! craft over the ramp, and an offscreen screenshot that is not a flat frame. No golden image.
//!
//! Every frame advances the clock by 50 ms, three 60 Hz fixed steps, and the run is paused with
//! explicit `play.step`s, so it does not depend on the machine's speed. A machine without any Vulkan
//! device (not even lavapipe) skips the test with a message.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};
use vehicle_lab::VehicleLabPlugin;

/// A hang guard only; nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the first offscreen frame may take, ms: a hang guard, inside `SCRIPT_TIMEOUT`.
const SCREENSHOT_TIMEOUT_MS: u64 = 400_000;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
/// Fixed steps per frame the 50 ms clock gives.
const STEPS_PER_FRAME: u32 = 3;

fn render_app(project: PathBuf) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        VehicleLabPlugin,
    ))
    .add_plugins(regia_agent::AgentPlugin)
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project),
    })
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app.world_mut().spawn(regia_platform::PrimaryWindow);
    app
}

/// One agent call. The protocol allows 50 mutating calls per second (`F0007`, "wait a second"), and
/// this script can outrun that, so it waits and retries; nothing asserts on time.
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

/// A call the protocol must refuse: its error text, after any wait for the rate limit.
fn refused(client: &mut Client, method: &str, params: Value) -> String {
    for _ in 0..100 {
        match client.call(method, params.clone()) {
            Ok(value) => panic!("{method} answered {value}"),
            Err(err) => {
                let text = format!("{err:?}");
                if !text.contains("F0007") {
                    return text;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    panic!("{method}: still rate limited after 100 tries");
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

/// Standard base64 to bytes.
fn base64_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut bits, mut acc) = (0u32, 0u32);
    for v in text.bytes().filter_map(value) {
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// The luma's standard deviation of an offscreen screenshot. The first frame builds every pipeline,
/// which takes lavapipe well over the default 30 s on a loaded machine, so this asks for a longer
/// answer deadline, as `F0006`'s hint says. Nothing asserts on the time; the print is for a reader.
fn screenshot_contrast(client: &mut Client) -> f64 {
    client
        .set_read_timeout(Some(SCRIPT_TIMEOUT))
        .expect("read timeout");
    let started = Instant::now();
    let answer = call(
        client,
        "seeing.screenshot",
        json!({
            "target": "offscreen",
            "width": WIDTH,
            "height": HEIGHT,
            "hud": false,
            "timeout_ms": SCREENSHOT_TIMEOUT_MS,
        }),
    );
    eprintln!(
        "the first offscreen frame took {:.1} s",
        started.elapsed().as_secs_f32()
    );
    let bytes = base64_decode(answer["png_base64"].as_str().unwrap_or_default());
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a PNG");
    let mut pixels = vec![0; reader.output_buffer_size().expect("buffer size")];
    let info = reader.next_frame(&mut pixels).expect("a frame");
    assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
    pixels.truncate(info.buffer_size());
    let lumas: Vec<f64> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect();
    let n = lumas.len().max(1) as f64;
    let mean = lumas.iter().sum::<f64>() / n;
    (lumas.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / n).sqrt()
}

fn drive_script(client: &mut Client) {
    call(
        client,
        "agent.hello",
        json!({ "client": "vehicle-lab", "model": "worker" }),
    );

    // The vehicles exist from the first frames; find them, then pause: every step from here on is
    // an explicit `play.step`.
    let mut listed = Value::Null;
    for _ in 0..600 {
        listed = call(client, "physics.vehicles", json!({}));
        if listed["count"] == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(listed["count"], 1, "the lab has one car: {listed}");
    let car = listed["vehicles"][0]["entity"].as_u64().expect("car id");
    let craft = call(client, "physics.hovercraft", json!({}));
    assert_eq!(craft["count"], 1, "the lab has one hover craft: {craft}");
    let craft = craft["hovercraft"][0]["entity"].as_u64().expect("craft id");
    call(client, "play.pause", json!({}));

    let idle = call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
    assert_eq!(idle["vehicles"][0]["drivetrain"], "geared", "{idle}");
    assert_eq!(idle["vehicles"][0]["gear"], 1, "{idle}");
    call(client, "play.step", json!({ "frames": 40 }));

    // A scripted launch: full throttle with a small steering weave, 60 fixed steps at a time.
    call(
        client,
        "physics.vehicle_input",
        json!({ "entity": craft, "throttle": 0.25 }),
    );
    let mut upshifts = 0;
    let mut rpm_seen = 0.0_f64;
    let mut gears = Vec::new();
    for second in 0..24 {
        let steer = 0.02 * f64::from(second % 4) - 0.03;
        call(
            client,
            "physics.vehicle_input",
            json!({ "entity": car, "throttle": 1.0, "steer": steer }),
        );
        call(client, "play.step", json!({ "frames": 60 / STEPS_PER_FRAME }));
        let telemetry = call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
        let row = &telemetry["vehicles"][0];
        assert!(row["error"].is_null(), "{row}");
        upshifts = row["upshifts"].as_u64().expect("upshifts");
        rpm_seen = rpm_seen.max(number(&row["rpm"]));
        gears.push(row["gear"].as_i64().expect("gear"));
    }
    assert!(
        upshifts >= 2,
        "at least two upshifts, saw {upshifts}: gears {gears:?}"
    );
    assert!(
        gears.windows(2).all(|pair| pair[1] >= pair[0]),
        "gears only rise: {gears:?}"
    );
    assert!(rpm_seen > 5000.0, "{rpm_seen}");
    let shot = screenshot_contrast(client);
    assert!(shot > 4.0, "not a flat frame: luma deviation {shot:.2}");

    let fast = call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
    let row = &fast["vehicles"][0];
    let speed = number(&row["speed"]);
    assert!(speed > 25.0, "{row}");
    let wheels = row["wheels"].as_array().expect("wheels");
    assert_eq!(wheels.len(), 4);
    assert!(wheels.iter().all(|wheel| wheel["grounded"] == true), "{row}");

    // The driver takes the gears, then hands them back.
    let top = row["gear"].as_i64().expect("gear");
    let held = call(
        client,
        "physics.vehicle_shift",
        json!({ "entity": car, "gear": top - 1 }),
    );
    assert_eq!(held["automatic"], false, "{held}");
    let error = refused(
        client,
        "physics.vehicle_shift",
        json!({ "entity": car, "gear": 99 }),
    );
    assert!(error.contains("vehicle_invalid_gear"), "{error}");
    call(client, "play.step", json!({ "frames": 20 }));
    let telemetry = call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
    assert_eq!(telemetry["vehicles"][0]["gear"], top - 1, "{telemetry}");
    call(
        client,
        "physics.vehicle_shift",
        json!({ "entity": car, "automatic": true }),
    );

    // A full brake stops the car with ABS on: it stays in control and comes to rest.
    call(
        client,
        "physics.vehicle_input",
        json!({ "entity": car, "throttle": 0.0, "brake": 1.0, "steer": 0.0 }),
    );
    let mut stopped = false;
    for _ in 0..12 {
        call(client, "play.step", json!({ "frames": 20 }));
        let report = call(client, "physics.vehicle_telemetry", json!({ "entity": car }));
        if number(&report["vehicles"][0]["speed"]) < 0.5 {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "the car stops within 12 s");

    // The hover craft crossed the ramp and kept going.
    let report = call(client, "physics.hovercraft", json!({ "entity": craft }));
    let row = &report["hovercraft"][0];
    assert!(row["last_error"].is_null(), "{row}");
    assert!(
        number(&row["position"][2]) < -60.0,
        "past the ramp at z -40: {row}"
    );
    assert!(number(&row["forward_speed"]) > 5.0, "{row}");

    call(client, "play.resume", json!({}));
}

#[test]
fn an_agent_drives_the_lab_and_sees_at_least_two_upshifts() {
    if let Err(err) = regia_render::RenderDevice::headless() {
        eprintln!("skipped: no Vulkan device for the offscreen capture ({err})");
        return;
    }
    let project = regia_test_utils::unique_temp_dir("regia-vehicle-lab");
    let _ = std::fs::create_dir_all(&project);
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let token_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);
    let script = std::thread::Builder::new()
        .name("vehicle-lab".into())
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
            drive_script(&mut client);
            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = render_app(project.clone());
    app.finish_plugins();
    while !script.is_finished() && Instant::now() < deadline {
        app.update();
    }
    let joined = script.join();
    let _ = std::fs::remove_dir_all(&project);
    assert!(joined.is_ok(), "the agent script panicked (see above)");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the agent script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
}
