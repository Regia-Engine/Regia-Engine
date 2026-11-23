// SPDX-License-Identifier: MIT-0
//! Agent-protocol reachability for the arena's match flow (`docs/specs/14` M20 `samples/arena`).
//!
//! Headless: `MinimalPlugins` plus the sample and the agent server, driven by a client thread
//! that opens a real agent connection and executes the `play.expect` assertions documented in
//! `book/agents/arena.md` — a bot is alive, the match is in the round, and the score vector is
//! zeroed. This proves the spec's "passes its agent-protocol `expect` checks" acceptance.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia::prelude::*;
use regia_agent::client::Client;
use regia_agent::endpoint;
use regia_audio::AudioSettings;
use regia_framework::settings::SettingsPath;
use serde_json::{Value, json};

use arena::{ArenaConfig, ArenaSample, ArenaState, MatchRng, Phase};

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(60);

/// A headless arena app with the agent server (the client thread drives it through the protocol).
fn app(project: PathBuf) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    // Two bots, no local player, no warm-up: the first fixed tick reaches the round.
    app.insert_resource(ArenaConfig {
        bot_count: 2,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 180.0,
        use_navmesh: false,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(0xA6E5));
    app.add_plugins((MinimalPlugins, ArenaSample))
        .add_plugins(regia_agent::AgentPlugin)
        .insert_resource(regia_agent::AgentSettings {
            project_dir: Some(project),
        });
    // The HUD and input systems read these platform messages even headless (no window plugin).
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    enable_fixed_step(&mut app, 60.0);
    app
}

fn call(client: &mut Client, method: &str, params: Value) -> Value {
    match client.call(method, params) {
        Ok(value) => value,
        Err(err) => panic!("{method} failed: {err:?}"),
    }
}

fn wait_until(client: &mut Client, condition: Value) -> Value {
    let params = json!({ "condition": condition, "timeout_ms": 20_000 });
    match client.call("play.wait_until", params) {
        Ok(value) => value,
        Err(err) => panic!("play.wait_until {condition} failed: {err:?}"),
    }
}

fn read_token(project: &std::path::Path) -> String {
    std::fs::read_to_string(endpoint::agent_dir(project).join("token")).expect("token file")
}

#[test]
fn agent_expect_asserts_the_match_state() {
    let project = std::env::temp_dir().join(format!("regia-m20-arena-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&project);
    std::fs::create_dir_all(&project).expect("project dir");

    let script_ok = Arc::new(AtomicBool::new(false));
    let flag = script_ok.clone();
    let script_project = project.clone();
    let agent_dir = endpoint::agent_dir(&project);

    let script = std::thread::Builder::new()
        .name("m20-arena-agent-client".into())
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
                json!({ "client": "m20-arena-expect", "model": "worker" }),
            );

            // Warm-up is 0 s, so the first fixed tick reaches the round.
            let round = wait_until(
                &mut client,
                json!({
                    "entity": { "component": "ArenaState" },
                    "component": "ArenaState",
                    "path": "phase",
                    "op": "eq",
                    "value": "Round"
                }),
            );
            assert_eq!(round["satisfied"], true, "match reached the round: {round}");

            // The book's `play.expect` assertions (book/agents/arena.md): a bot is alive, the
            // match is in the round, and the score vector is zeroed (three slots: two bots + the
            // empty local slot).
            let report = call(
                &mut client,
                "play.expect",
                json!({ "assertions": [
                    { "name": "a bot is alive",
                      "condition": { "entity": { "component": "Pawn" }, "component": "Health",
                                     "path": "current", "op": "gt", "value": 0.0 } },
                    { "name": "the match is in the round",
                      "condition": { "entity": { "component": "ArenaState" }, "component": "ArenaState",
                                     "path": "phase", "op": "eq", "value": "Round" } },
                    { "name": "scores start at zero",
                      "condition": { "entity": { "component": "ArenaState" }, "component": "ArenaState",
                                     "path": "scores", "op": "eq", "value": [0, 0, 0] } }
                ]}),
            );
            assert_eq!(report["pass"], true, "play.expect assertions failed: {report}");

            flag.store(true, Ordering::Relaxed);
        })
        .expect("client thread");

    let deadline = Instant::now() + SCRIPT_TIMEOUT;
    let mut app = app(project.clone());
    app.finish_plugins();
    // The bots fight from the first round tick and the score limit is huge, so "scores start at zero"
    // would race the client thread (a starved client on a loaded host saw 2-2). Freeze the clock the
    // moment the round starts, as `play.pause` does; the protocol still answers while paused.
    let reached_round = |app: &mut App| {
        let mut states = app.world_mut().query::<&ArenaState>();
        states.iter(app.world()).any(|s| s.phase == Phase::Round)
    };
    while !reached_round(&mut app) && Instant::now() < deadline {
        app.update();
    }
    app.world_mut().resource_mut::<regia_app::Time>().paused = true;
    while !script_ok.load(Ordering::Relaxed) && Instant::now() < deadline {
        app.update();
    }
    script.join().expect("script finished");
    assert!(
        script_ok.load(Ordering::Relaxed),
        "agent expect script did not finish within {} s",
        SCRIPT_TIMEOUT.as_secs()
    );
    let _ = std::fs::remove_dir_all(&project);
}
