// SPDX-License-Identifier: MIT-0
//! The state-tree arena rendered headless and driven through the agent protocol (ADR-2134, rule 8):
//! `play.wait_until` (a bot reaches `Fight`), `play.expect` (the brain, the stations, the spectator
//! view), `seeing.screenshot` (offscreen, lavapipe in CI) through the spectator camera, and a second
//! shot after `scene.set_property` moves the spectator to another bot. No golden image: the checks
//! are on the pixels (not flat, not clipped, the two views differ).
//!
//! Each frame advances the clock by exactly one fixed step (`TimeUpdateStrategy::ManualDuration`),
//! so the frames the script sees do not depend on how fast the machine runs. A machine without any
//! Vulkan device (not even lavapipe) skips the test with a message.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use serde_json::{Value, json};

use arena::{ArenaConfig, ArenaSample, BotBrainKind, MatchRng};

/// A hang guard only; nothing asserts on time. Holds [`SHOT_TIMEOUT_MS`] plus the rest of the script.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);
/// The `timeout_ms` each `seeing.screenshot` passes (ADR-7704): a hang guard only, as ADR-7640
/// did for the procgen samples. The first offscreen capture builds its pipelines, and on a cold
/// driver cache it passed the server's 30 s default (the v0.5.0 rc5 GPU sweep: F0006 on an RTX 5090
/// at load 100-130). Measured on the build server with an empty driver shader cache: RTX 5090 20.3-28.2 s
/// at load 50-65, 34.0-39.8 s at load 72-91, 25.8-34.7 s pinned to one core; Intel Arc 50.7-60.3 s
/// at load 72-91; lavapipe 49.7-56.8 s at load 50-65, 76.9-107.4 s at load 72-91, 62-74 s pinned to
/// two cores, 96.8 s pinned to one. A warm cache answers in 1.1-18.9 s.
const SHOT_TIMEOUT_MS: u64 = 480_000;
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;

fn render_app(project: PathBuf) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: 3,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 600.0,
        use_navmesh: false,
        brain: BotBrainKind::StateTree,
    });
    app.insert_resource(MatchRng::new(0x5EE));
    app.add_plugins((
        MinimalPlugins,
        regia_render::RenderPlugin { headless: true },
        ArenaSample,
        regia_agent::AgentPlugin,
    ))
    .insert_resource(regia_agent::AgentSettings {
        project_dir: Some(project),
    });
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_nanos(
        1_000_000_000 / 60,
    )));
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

/// An offscreen screenshot (no HUD) as RGBA8 pixels.
fn shot(client: &mut Client) -> Vec<u8> {
    let started = Instant::now();
    let answer = call(
        client,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": WIDTH, "height": HEIGHT, "hud": false,
                "timeout_ms": SHOT_TIMEOUT_MS }),
    );
    // Printed for the record (nothing asserts on time): how long this host took.
    eprintln!(
        "seeing.screenshot {WIDTH}x{HEIGHT} answered in {} ms",
        started.elapsed().as_millis()
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

#[test]
fn an_agent_watches_the_state_tree_match_through_the_spectator_camera() {
    if let Err(err) = regia_render::RenderDevice::headless() {
        eprintln!("skipped: no Vulkan device for the offscreen capture ({err})");
        return;
    }
    let project = regia_test_utils::unique_temp_dir("regia-wc-arena-render");
    let _ = std::fs::create_dir_all(&project);
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("wc-arena-render-client".into())
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
            call(&mut client, "agent.hello", json!({ "client": "wc-arena-render", "model": "worker" }));

            // Wait for the first bot's tree to reach Fight (tick 2 of the round).
            let fight = call(
                &mut client,
                "play.wait_until",
                json!({ "condition": { "entity": { "component": "StateTreeStatus" }, "component": "StateTreeStatus",
                                       "path": "active_path[1]", "op": "eq", "value": "Fight" },
                        "timeout_ms": 120_000 }),
            );
            assert_eq!(fight["satisfied"], true, "{fight}");
            call(&mut client, "play.pause", json!({}));

            let report = call(
                &mut client,
                "play.expect",
                json!({ "assertions": [
                    { "name": "state-tree brain",
                      "condition": { "entity": { "component": "ArenaBrainStatus" }, "component": "ArenaBrainStatus",
                                     "path": "brain", "op": "eq", "value": "state_tree" } },
                    { "name": "three trees",
                      "condition": { "entity": { "component": "ArenaBrainStatus" }, "component": "ArenaBrainStatus",
                                     "path": "trees", "op": "eq", "value": 3 } },
                    { "name": "two stations",
                      "condition": { "entity": { "component": "ArenaBrainStatus" }, "component": "ArenaBrainStatus",
                                     "path": "stations", "op": "eq", "value": 2 } },
                    { "name": "spectator view on",
                      "condition": { "entity": { "name": "main_camera" }, "component": "SpectatorFocus",
                                     "path": "enabled", "op": "eq", "value": true } },
                    { "name": "following bot 1",
                      "condition": { "entity": { "component": "SpectatorFocus" }, "component": "SpectatorFocus",
                                     "path": "slot", "op": "eq", "value": 1 } }
                ]}),
            );
            assert_eq!(report["pass"], true, "{report}");

            // The spectator view of bot 1: a lit arena, neither flat nor clipped.
            let first = shot(&mut client);
            let (mean, deviation) = luma_stats(&first);
            assert!(mean > 8.0 && mean < 240.0, "exposed (mean luma {mean:.1})");
            assert!(deviation > 4.0, "not a flat frame (luma deviation {deviation:.2})");

            // Follow bot 3 instead: the camera moves to the other bot, so the frame changes.
            let camera = call(
                &mut client,
                "scene.query_entities",
                json!({ "component": "SpectatorFocus", "limit": 1 }),
            )["entities"][0]["entity"]
                .as_u64()
                .unwrap_or_else(|| panic!("a SpectatorFocus camera"));
            call(
                &mut client,
                "scene.set_property",
                json!({ "entity": camera, "component": "SpectatorFocus", "path": "slot", "value": 3 }),
            );
            call(&mut client, "play.step", json!({ "frames": 30 }));
            let report = call(
                &mut client,
                "play.expect",
                json!({ "assertions": [
                    { "name": "following bot 3",
                      "condition": { "entity": { "component": "SpectatorFocus" }, "component": "SpectatorFocus",
                                     "path": "slot", "op": "eq", "value": 3 } }
                ]}),
            );
            assert_eq!(report["pass"], true, "{report}");
            let spectator = call(&mut client, "spectator.get", json!({}));
            assert_eq!(spectator["mode"], "follow", "{spectator}");
            let second = shot(&mut client);
            let changed = first
                .as_chunks::<4>()
                .0
                .iter()
                .zip(second.as_chunks::<4>().0)
                .filter(|(a, b)| a != b)
                .count();
            let total = (WIDTH * HEIGHT) as usize;
            assert!(
                changed * 20 > total,
                "the view follows another bot ({changed} of {total} pixels changed)"
            );
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
