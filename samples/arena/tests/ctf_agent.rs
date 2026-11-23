// SPDX-License-Identifier: MIT-0
//! The capture-the-flag tutorial's agent steps (`book/tutorials/capture-the-flag.md`, "Drive it with
//! the agent"), run against a headless arena in CTF mode (rule 8, GR-8.3): `play.expect` on
//! `ArenaModeStatus`, `rules.match_get` on the hosted session, `play.step`, an offscreen
//! `seeing.screenshot` (lavapipe in CI), and `play.wait_until` the end of the match. No golden image:
//! the frames are checked for content and, when `ARENA_CTF_EVIDENCE_DIR` names a directory, saved
//! there as evidence (the paths are printed).
//!
//! Each frame advances the clock by exactly one fixed step (`TimeUpdateStrategy::ManualDuration`),
//! so the match the script sees does not depend on how fast the machine runs. A machine without any
//! Vulkan device (not even lavapipe) skips the test with a message.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_app::{App, MinimalPlugins, TimeUpdateStrategy};
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use serde_json::{Value, json};

use arena::{ArenaConfig, ArenaMatchMode, ArenaMode, ArenaSample, BotBrainKind, MatchRng};

/// A hang guard only; nothing asserts on time.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(900);
/// The `timeout_ms` each `seeing.screenshot` passes: a hang guard (a cold lavapipe pipeline build
/// takes up to about 100 s on a loaded host, see `wc_render_agent.rs`).
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
    // The tutorial's match: `--mode ctf --limit 1 --no-local-player --bots 4 --seed 7 --warmup 0`.
    app.insert_resource(ArenaConfig {
        bot_count: 4,
        local_player: false,
        warmup_secs: 0.0,
        match_secs: 180.0,
        brain: BotBrainKind::Steering,
        ..ArenaConfig::default()
    });
    app.insert_resource(ArenaMatchMode {
        limit: Some(1),
        ..ArenaMatchMode::new(ArenaMode::Ctf)
    });
    app.insert_resource(MatchRng::new(7));
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

fn read_token(project: &Path) -> String {
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

/// An offscreen screenshot saved as `<dir>/<name>.png`; returns the luma deviation (0..255).
fn shot(client: &mut Client, dir: &Path, name: &str) -> f64 {
    let answer = call(
        client,
        "seeing.screenshot",
        json!({ "target": "offscreen", "width": WIDTH, "height": HEIGHT, "hud": false,
                "timeout_ms": SHOT_TIMEOUT_MS }),
    );
    assert_eq!(answer["width"], WIDTH, "{answer}");
    let bytes = base64_decode(answer["png_base64"].as_str().unwrap_or_default());
    let path = dir.join(format!("{name}.png"));
    let tmp = dir.join(format!("{name}.png.tmp"));
    if std::fs::write(&tmp, &bytes)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .is_ok()
    {
        eprintln!("evidence: {}", path.display());
    }
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

/// The two flags of a `rules.match_get` answer as `(entity_id, at_base, carrier_id)`.
fn flags(answer: &Value) -> Vec<(u64, bool, Option<u64>)> {
    let mut out: Vec<_> = answer["board"]["objectives"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|o| {
                    (
                        o["entity_id"].as_u64().unwrap_or(0),
                        o["at_base"].as_bool().unwrap_or(false),
                        o["carrier_id"].as_u64(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_unstable();
    out
}

#[test]
fn the_ctf_tutorial_steps_pass_through_the_agent() {
    if let Err(err) = regia_render::RenderDevice::headless() {
        eprintln!("skipped: no Vulkan device for the offscreen capture ({err})");
        return;
    }
    let project = regia_test_utils::unique_temp_dir("regia-arena-ctf-agent");
    let _ = std::fs::create_dir_all(&project);
    let evidence = std::env::var_os("ARENA_CTF_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join("evidence"));
    let _ = std::fs::create_dir_all(&evidence);
    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("arena-ctf-agent-client".into())
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
            call(&mut client, "agent.hello", json!({ "client": "arena-ctf-tutorial", "model": "worker" }));
            call(&mut client, "play.pause", json!({}));

            // 1. The mode and the rule set.
            let report = call(
                &mut client,
                "play.expect",
                json!({ "assertions": [
                    { "name": "ctf from the shipped rule set",
                      "condition": { "entity": { "component": "ArenaModeStatus" }, "component": "ArenaModeStatus",
                                     "path": "ruleset", "op": "eq", "value": "ctf" } },
                    { "name": "mode ctf",
                      "condition": { "entity": { "component": "ArenaModeStatus" }, "component": "ArenaModeStatus",
                                     "path": "mode", "op": "eq", "value": "ctf" } },
                    { "name": "mode bots",
                      "condition": { "entity": { "component": "ArenaBrainStatus" }, "component": "ArenaBrainStatus",
                                     "path": "brain", "op": "eq", "value": "mode" } }
                ]}),
            );
            assert_eq!(report["pass"], true, "{report}");

            // 2. The session: two flags at their bases, five seats.
            let session = call(&mut client, "rules.match_get", json!({}));
            assert_eq!(session["mode"], "ctf", "{session}");
            assert_eq!(flags(&session), vec![(901, true, None), (902, true, None)], "{session}");
            assert_eq!(
                session["flow"]["seated"].as_array().map(Vec::len),
                Some(5),
                "{session}"
            );
            let start_dev = shot(&mut client, &evidence, "ctf-start");
            assert!(start_dev > 4.0, "the start frame is not flat ({start_dev:.2})");

            // 3. Five seconds in, a flag is off its base.
            call(&mut client, "play.step", json!({ "frames": 300 }));
            let session = call(&mut client, "rules.match_get", json!({}));
            assert!(
                flags(&session).iter().any(|(_, at_base, _)| !at_base),
                "a flag left its base: {session}"
            );
            let play_dev = shot(&mut client, &evidence, "ctf-5s");
            assert!(play_dev > 4.0, "the play frame is not flat ({play_dev:.2})");

            // 4. The end: the first capture wins (`--limit 1`).
            call(&mut client, "play.resume", json!({}));
            let over = call(
                &mut client,
                "play.wait_until",
                json!({ "condition": { "entity": { "component": "ArenaState" }, "component": "ArenaState",
                                       "path": "phase", "op": "eq", "value": "MatchOver" },
                        "timeout_ms": 600_000 }),
            );
            assert_eq!(over["satisfied"], true, "{over}");
            call(&mut client, "play.pause", json!({}));
            let session = call(&mut client, "rules.match_get", json!({}));
            assert_eq!(session["board"]["end_reason"], "capture_limit_reached", "{session}");
            let scores = session["board"]["team_scores"].clone();
            assert!(
                scores.as_array().is_some_and(|s| s.iter().any(|v| v.as_u64() == Some(1))),
                "a team has the capture: {scores}"
            );
            shot(&mut client, &evidence, "ctf-end");
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
    // The evidence stays only in `ARENA_CTF_EVIDENCE_DIR`; the temp project (and any shots in it) goes.
    let _ = std::fs::remove_dir_all(&project);
    assert!(joined.is_ok(), "the agent script panicked (see above)");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "the agent script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
}
