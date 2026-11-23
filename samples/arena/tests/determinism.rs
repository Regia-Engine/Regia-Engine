// SPDX-License-Identifier: MIT-0
//! Determinism acceptance (`docs/specs/14` M20 `samples/arena`, `docs/06` § 6): a recorded
//! three-minute bot match replays to the same final score, and playback injects the recorded
//! actions.

use regia_app::{App, MinimalPlugins, enable_fixed_step};
use regia_audio::AudioSettings;
use regia_core::math::Vec3;
use regia_framework::input_recording::InputRecorder;
use regia_framework::settings::SettingsPath;
use regia_input::{Actions, InputInjection};

use arena::{ArenaConfig, ArenaSample, ArenaState, MatchRng};

const SEED: u64 = 0xDEAD_BEEF;
const MARK: Vec3 = Vec3::new(1.0, 0.5, 0.0);
/// Fixed steps in a three-minute match at 60 Hz.
const STEPS: usize = 180 * 60;

/// A headless arena app: `MinimalPlugins` (no rendering — the sim runs fast) + `ArenaSample`.
fn make_app(seed: u64) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.add_plugins((MinimalPlugins, ArenaSample));
    // The UI systems read the platform input messages; register them (no window in a test).
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    // A score limit high enough that the match runs the full three minutes.
    app.insert_resource(ArenaConfig {
        bot_count: 8,
        local_player: false,
        score_limit: 1_000_000,
        warmup_secs: 0.0,
        match_secs: 180.0,
        use_navmesh: false,
        brain: arena::BotBrainKind::Steering,
    });
    app.insert_resource(MatchRng::new(seed));
    enable_fixed_step(&mut app, 60.0);
    app
}

/// Records `STEPS` of the match. One scheduled `Move` is non-zero so playback can prove the
/// recorded actions are injected (a bot-only match otherwise records zeros).
fn record_match() -> (regia_framework::input_recording::InputRecording, ArenaState) {
    let mut app = make_app(SEED);
    {
        let world = app.world_mut();
        let mut recorder = world.remove_resource::<InputRecorder>().expect("recorder");
        recorder.record(world, SEED);
        world.insert_resource(recorder);
        world
            .resource_mut::<InputInjection>()
            .schedule("Move", MARK, 40, 1);
    }
    for _ in 0..STEPS {
        app.update();
    }
    app.world_mut().resource_mut::<InputRecorder>().stop();
    let recording = app.world().resource::<InputRecorder>().recording.clone();
    let state = arena::match_flow::arena_state(app.world())
        .expect("match state")
        .clone();
    (recording, state)
}

#[test]
fn three_minute_bot_match_replays_to_the_same_final_score() {
    let (recording, recorded) = record_match();
    assert_eq!(recording.seed, SEED);
    assert_eq!(recording.len(), STEPS, "one recorded step per fixed update");
    assert!(
        recording.frames.iter().any(|frame| frame.contains(&MARK)),
        "the recording contains the scheduled Move"
    );
    assert!(
        recorded.scores.iter().any(|&score| score > 0),
        "bots scored during the match: {:?}",
        recorded.scores
    );

    let mut replay = make_app(recording.seed);
    replay
        .world_mut()
        .resource_mut::<InputRecorder>()
        .play(recording.clone());
    let mut injected = false;
    for _ in 0..STEPS {
        replay.update();
        if replay.world().resource::<Actions>().value("Move") == MARK {
            injected = true;
        }
    }
    let played = arena::match_flow::arena_state(replay.world())
        .expect("match state")
        .clone();
    assert!(injected, "playback injects the recorded Move into Actions");
    assert_eq!(
        recorded.scores, played.scores,
        "replay of the recording matches the final score vector"
    );
}
