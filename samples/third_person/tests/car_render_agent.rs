// SPDX-License-Identifier: MIT-0
//! The third-person car rendered headless and driven through the agent protocol (ADR-2134, rule 8):
//! `play.wait_until` (the level is presented, the character settles), `play.expect` (the car, the
//! tags, the driver), `seeing.screenshot` (offscreen, lavapipe in CI) on foot and again through the
//! `vehicle_chase` camera-driver asset after driving off. No golden image: the checks are on the
//! pixels (not flat, not clipped, the chase view differs from the boom view).
//!
//! Each frame advances the clock by 16 ms (`TimeUpdateStrategy::ManualDuration`), and the driving
//! part is paused with explicit `play.step`s, so the run does not depend on the machine's speed. A
//! machine without any Vulkan device (not even lavapipe) skips the test with a message.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use serde_json::{Value, json};
use third_person::ThirdPersonSample;

/// A hang guard only; nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(300);
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

fn render_app(project: PathBuf) -> App {
    let mut app = App::new();
    // Straight into play (the title is covered by tests/combat.rs); saves and settings under the
    // temporary project.
    app.insert_resource(regia_framework::prelude::GameFlowSettings {
        skip_main_menu: true,
        ..Default::default()
    })
    .insert_resource(third_person::SampleDataDir(project.join(".data")));
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ThirdPersonSample,
    ))
    .add_plugins(regia_agent::AgentPlugin)
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project),
    })
    .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.world_mut().spawn(regia_platform::PrimaryWindow);
    app
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn read_token(project: &std::path::Path) -> String {
    std::fs::read_to_string(endpoint::agent_dir(project).join("token")).expect("token file")
}

fn wait_until(client: &mut Client, condition: Value) -> Value {
    call(
        client,
        "play.wait_until",
        json!({ "condition": condition, "timeout_ms": 120_000 }),
    )
}

fn key(client: &mut Client, key: &str, state: &str) {
    call(
        client,
        "play.inject_input",
        json!({ "events": [{ "type": "key", "key": key, "state": state }] }),
    );
}

fn entity_with(client: &mut Client, component: &str) -> u64 {
    let listed = call(
        client,
        "scene.query_entities",
        json!({ "component": component, "limit": 4 }),
    );
    listed["entities"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("entity"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("no entity with {component}: {listed}"))
}

fn set_f32(client: &mut Client, entity: u64, component: &str, path: &str, value: f32) {
    call(
        client,
        "scene.set_property",
        json!({ "entity": entity, "component": component, "path": path, "value": value }),
    );
}

/// Standard base64 (the screenshot's `png_base64`) to bytes.
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

/// An offscreen screenshot (no HUD) as RGBA8 pixels. On the CPU rasterizer of a loaded host the
/// first one took longer than the protocol's default 30 s answer wait (the build server, 11-05, at the base
/// commit too), so the call names its own wait.
fn shot(client: &mut Client) -> Vec<u8> {
    let answer = call(
        client,
        "seeing.screenshot",
        json!({
            "target": "offscreen",
            "width": WIDTH,
            "height": HEIGHT,
            "hud": false,
            "timeout_ms": 300_000,
        }),
    );
    assert_eq!(answer["width"], WIDTH, "{answer}");
    let bytes = base64_decode(answer["png_base64"].as_str().unwrap_or_default());
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("a PNG");
    let mut pixels = vec![0; reader.output_buffer_size().expect("buffer size")];
    let info = reader.next_frame(&mut pixels).expect("a frame");
    assert_eq!((info.width, info.height), (WIDTH, HEIGHT));
    assert_eq!(info.color_type, png::ColorType::Rgba, "RGBA8 capture");
    pixels.truncate(info.buffer_size());
    pixels
}

/// Mean and standard deviation of the Rec. 709 luma (0..255).
fn luma_stats(rgba: &[u8]) -> (f64, f64) {
    let lumas: Vec<f64> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect();
    let n = lumas.len().max(1) as f64;
    let mean = lumas.iter().sum::<f64>() / n;
    let var = lumas.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / n;
    (mean, var.sqrt())
}

fn assert_readable(rgba: &[u8], what: &str) {
    let (mean, deviation) = luma_stats(rgba);
    assert!(
        mean > 8.0 && mean < 240.0,
        "{what}: exposed (mean luma {mean:.1})"
    );
    assert!(
        deviation > 4.0,
        "{what}: not a flat frame (luma deviation {deviation:.2})"
    );
}

#[test]
fn an_agent_drives_the_car_and_sees_it_through_the_chase_camera() {
    if let Err(err) = regia_render::RenderDevice::headless() {
        eprintln!("skipped: no Vulkan device for the offscreen capture ({err})");
        return;
    }
    let project = regia_test_utils::unique_temp_dir("regia-wc-third-person-render");
    let _ = std::fs::create_dir_all(&project);
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("wc-third-person-render".into())
        .spawn(move || {
            let mut client = None;
            for _ in 0..2000 {
                if let Ok((advertised, _token)) = endpoint::read(&agent_dir)
                    && let Ok(connected) = Client::connect(&advertised.url, &read_token(&script_project))
                {
                    client = Some(connected);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let Some(mut client) = client else {
                panic!("no endpoint at {}", agent_dir.display());
            };
            call(
                &mut client,
                "agent.hello",
                json!({ "client": "wc-third-person-render", "model": "worker" }),
            );

            let presented = wait_until(
                &mut client,
                json!({ "entity": { "component": "SampleState" }, "component": "SampleState",
                        "path": "total", "op": "eq", "value": 8 }),
            );
            assert_eq!(presented["satisfied"], true, "level presented: {presented}");
            let grounded = wait_until(
                &mut client,
                json!({ "entity": { "component": "Character" }, "component": "CharacterState",
                        "path": "grounded", "op": "eq", "value": true }),
            );
            assert_eq!(grounded["satisfied"], true, "settled: {grounded}");

            let character = entity_with(&mut client, "CharacterState");
            let car = entity_with(&mut client, "Car");
            let tags = call(&mut client, "tags.get", json!({ "entity": car }));
            assert_eq!(tags["tags"], json!(["Vehicle.Car"]), "{tags}");

            // Paused from here: every frame is an explicit `play.step`.
            call(&mut client, "play.pause", json!({}));
            let start = call(
                &mut client,
                "scene.get_entity",
                json!({ "entity": car, "components": ["Transform"] }),
            );
            let at = &start["components"]["Transform"]["value"]["translation"];
            let (x, z) = (
                at[0].as_f64().unwrap_or(-12.0) as f32,
                at[2].as_f64().unwrap_or(-10.0) as f32,
            );
            for axis in ["x", "y", "z"] {
                set_f32(&mut client, character, "CharacterState", &format!("velocity.{axis}"), 0.0);
            }
            set_f32(&mut client, character, "Transform", "translation.x", x + 2.4);
            set_f32(&mut client, character, "Transform", "translation.y", 1.3);
            set_f32(&mut client, character, "Transform", "translation.z", z);
            call(&mut client, "play.step", json!({ "frames": 60 }));
            let on_foot = shot(&mut client);
            assert_readable(&on_foot, "on foot beside the car");

            key(&mut client, "KeyE", "pressed");
            call(&mut client, "play.step", json!({ "frames": 2 }));
            key(&mut client, "KeyE", "released");
            call(&mut client, "play.step", json!({ "frames": 2 }));
            let report = call(
                &mut client,
                "play.expect",
                json!({ "assertions": [
                    { "name": "driving",
                      "condition": { "entity": character, "component": "Driving", "path": "car", "op": "eq", "value": car } }
                ]}),
            );
            assert_eq!(report["pass"], true, "{report}");
            let camera = entity_with(&mut client, "PlayerCamera");
            let inspect = call(&mut client, "camera.inspect", json!({ "entity": camera }));
            assert!(inspect.to_string().contains("vehicle_chase"), "{inspect}");

            // Drive off for two seconds and look through the chase camera.
            call(
                &mut client,
                "physics.vehicle_input",
                json!({ "entity": car, "throttle": 1.0 }),
            );
            call(&mut client, "play.step", json!({ "frames": 120 }));
            let moved = call(&mut client, "physics.vehicles", json!({ "entity": car }));
            let speed = moved["vehicles"][0]["forward_speed"].as_f64().unwrap_or(0.0);
            assert!(speed > 1.0, "the car drives forward: {moved}");
            let driving = shot(&mut client);
            assert_readable(&driving, "the chase view");
            let changed = on_foot
                .as_chunks::<4>()
                .0
                .iter()
                .zip(driving.as_chunks::<4>().0)
                .filter(|(a, b)| a != b)
                .count();
            let total = (WIDTH * HEIGHT) as usize;
            assert!(
                changed * 10 > total,
                "the chase camera shows another view ({changed} of {total} pixels changed)"
            );
            call(&mut client, "play.resume", json!({}));
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
