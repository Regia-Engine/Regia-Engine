// SPDX-License-Identifier: MIT-0
//! Team modes through the rule-set registry (GR-8.3, `book/tutorials/capture-the-flag.md`).
//!
//! `arena_ctf_bots_finish_a_match_headless` is the spec 41 functional test `arena_ctf_bots`
//! (`arena::smoke::install_ctf`) run in process: two bots per side play the shipped `ctf` rule set
//! through the hosted match session (`MatchSessionPlugin`) until a capture ends the match. No
//! window, no clock: the run is bounded by a frame count.
#![cfg(feature = "agent")]

use regia_app::{App, MinimalPlugins, enable_fixed_step};
use regia_audio::AudioSettings;
use regia_framework::functional::report::EXIT_PASSED;
use regia_framework::functional::{RunOptions, Runner, RunnerStatus, Selection};
use regia_framework::rules::mode::MatchMode;
use regia_framework::rules::phases::{PhaseSettings, TimeUp};
use regia_framework::rules::{MatchRules, ObjectiveEntity, TeamId};
use regia_framework::settings::SettingsPath;

use arena::{ArenaConfig, ArenaMatchMode, ArenaMode, ArenaSample, BotBrainKind, MatchRng, Pawn};

fn ctf_app(seed: u64, limit: Option<u32>) -> App {
    mode_app(ArenaMode::Ctf, seed, limit)
}

fn mode_app(mode: ArenaMode, seed: u64, limit: Option<u32>) -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: 4,
        local_player: false,
        warmup_secs: 0.0,
        match_secs: 180.0,
        brain: BotBrainKind::Steering,
        ..ArenaConfig::default()
    });
    app.insert_resource(ArenaMatchMode {
        limit,
        ..ArenaMatchMode::new(mode)
    });
    app.add_plugins((MinimalPlugins, ArenaSample));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(MatchRng::new(seed));
    enable_fixed_step(&mut app, 60.0);
    arena::smoke::install_ctf(&mut app);
    app
}

/// Team ids of the bot pawns, sorted.
fn bot_teams(app: &mut App) -> Vec<u8> {
    let mut query = app.world_mut().query::<(&Pawn, &TeamId)>();
    let mut teams: Vec<u8> = query
        .iter(app.world())
        .filter(|(pawn, _)| pawn.is_bot)
        .map(|(_, team)| team.0)
        .collect();
    teams.sort_unstable();
    teams
}

#[test]
fn arena_ctf_bots_finish_a_match_headless() {
    let mut app = ctf_app(7, Some(1));
    // One frame runs Startup: the arena, the team starts, the flags and the placed test.
    app.update();
    assert_eq!(bot_teams(&mut app), vec![1, 1, 2, 2], "two bots per side");
    {
        let session = &app.world().resource::<MatchRules>().session;
        assert_eq!(session.board.mode, MatchMode::CaptureTheFlag, "the ctf rule set");
    }
    let markers = app
        .world_mut()
        .query::<&ObjectiveEntity>()
        .iter(app.world())
        .count();
    assert_eq!(markers, 2, "one flag entity per team");

    app.world_mut().resource_mut::<Runner>().start(
        Selection {
            name_globs: Vec::new(),
            tags: vec!["ctf".into()],
        },
        RunOptions::default(),
    );
    let budget = arena::smoke::CTF_FRAME_LIMIT + 600;
    for _ in 0..budget {
        app.update();
        if app.world().resource::<Runner>().status() == RunnerStatus::Done {
            break;
        }
    }
    let runner = app.world().resource::<Runner>();
    assert_eq!(runner.status(), RunnerStatus::Done, "the run finished");
    let report = runner.report().clone();
    assert_eq!(report.tests.len(), 1, "{report:?}");
    assert_eq!(report.tests[0].name, "arena_ctf_bots");
    assert_eq!(report.exit_code(), EXIT_PASSED, "{report:?}");

    let session = &app.world().resource::<MatchRules>().session;
    let captures: u32 = session.ledgers.rows.iter().map(|row| row.objective_points).sum();
    assert!(
        captures >= 1,
        "a carrier's row has the capture: {:?}",
        session.ledgers.rows
    );
    let state = arena::match_flow::arena_state(app.world()).expect("match state");
    assert_eq!(
        state.phase,
        arena::Phase::MatchOver,
        "the arena view shows the end"
    );
}

#[test]
fn the_ctf_match_is_the_same_on_two_runs() {
    let run = |seed: u64| {
        let mut app = ctf_app(seed, Some(1));
        for _ in 0..60 * 30 {
            app.update();
        }
        let session = &app.world().resource::<MatchRules>().session;
        (
            session.board.team_scores.clone(),
            session.board.objectives.clone(),
            session.ledgers.rows.clone(),
        )
    };
    assert_eq!(run(11), run(11), "same seed, same match");
}

/// `--mode koth`: the shipped `koth` rule set, the hill in the middle, and the mode bots contest it.
/// A point needs one team alone on the hill at an accrual tick (once a second), which a 2v2 of
/// equal bots seldom allows, so the limit here is one point: the hill, not the timer, ends it.
#[test]
fn arena_koth_bots_score_on_the_hill() {
    let mut app = mode_app(ArenaMode::Koth, 7, Some(1));
    for _ in 0..60 * 120 {
        app.update();
        if app.world().resource::<MatchRules>().session.board.match_over {
            break;
        }
    }
    let session = &app.world().resource::<MatchRules>().session;
    assert_eq!(session.board.mode, MatchMode::KingOfTheHill);
    assert!(
        session.board.match_over,
        "the hill decided the match: {:?}",
        session.board.team_scores
    );
    assert_eq!(
        session.board.end_reason,
        Some(regia_framework::rules::objective::EndReason::HillScoreLimitReached)
    );
    assert!(session.board.team_scores.iter().any(|&s| s >= 1));
}

/// `--mode tdm`: the shipped `tdm` rule set, no objectives; the arena reports the first team to
/// `ArenaConfig::score_limit` kills (here 2) and the session ends the match on it.
#[test]
fn arena_tdm_bots_end_on_the_team_kill_limit() {
    let mut app = mode_app(ArenaMode::Tdm, 7, None);
    app.world_mut().resource_mut::<ArenaConfig>().score_limit = 2;
    for _ in 0..60 * 120 {
        app.update();
        if app.world().resource::<MatchRules>().session.board.match_over {
            break;
        }
    }
    let session = &app.world().resource::<MatchRules>().session;
    assert_eq!(session.board.mode, MatchMode::TeamDeathmatch);
    assert!(session.board.match_over, "the kill limit ended the match");
    assert_eq!(
        session.board.end_reason,
        Some(regia_framework::rules::objective::EndReason::Ended),
        "the arena's reported condition, not the timer"
    );
    assert!(
        arena::modes::team_kills(session).iter().any(|&k| k >= 2),
        "{:?}",
        session.ledgers.rows
    );
}

/// The documented tutorial command
/// (`cargo run -p arena -- --mode ctf --limit 1 --no-local-player --bots 4 --seed 7 --warmup 0`)
/// skips the default 3 s warm-up (`--warmup 0`, `ArenaConfig::warmup_secs`), so the match is
/// live at once. After `play.step` of 300 frames the tutorial says a flag has left its base.
/// This match is paused at the start, then stepped 300 fixed frames, the same window the page
/// describes.
fn tutorial_command_app() -> App {
    let mut app = App::new();
    app.insert_resource(AudioSettings {
        headless: true,
        ..Default::default()
    });
    app.insert_resource(SettingsPath(None));
    app.insert_resource(ArenaConfig {
        bot_count: 4,
        local_player: false,
        warmup_secs: 0.0,
        ..ArenaConfig::default()
    });
    app.insert_resource(ArenaMatchMode {
        limit: Some(1),
        ..ArenaMatchMode::new(ArenaMode::Ctf)
    });
    app.add_plugins((MinimalPlugins, ArenaSample));
    app.add_message::<regia_platform::WindowResized>()
        .add_message::<regia_platform::WindowScaleFactorChanged>()
        .add_message::<regia_platform::CursorMoved>()
        .add_message::<regia_platform::MouseButtonInput>()
        .add_message::<regia_platform::TouchInput>()
        .add_message::<regia_platform::MouseWheel>()
        .add_message::<regia_platform::KeyboardInput>()
        .add_message::<regia_platform::ImeEvent>();
    app.insert_resource(MatchRng::new(7));
    enable_fixed_step(&mut app, 60.0);
    app
}

#[test]
fn the_tutorial_command_has_a_flag_off_base_after_300_frames() {
    let mut app = tutorial_command_app();
    // Startup builds the match. The following 300 updates are `play.step {"frames":300}`.
    app.update();
    for _ in 0..300 {
        app.update();
    }
    let session = &app.world().resource::<MatchRules>().session;
    let flags: Vec<(u32, bool, Option<u32>)> = session
        .board
        .objectives
        .iter()
        .filter(|o| o.is_flag())
        .map(|o| (o.entity_id, o.at_base, o.carrier_id))
        .collect();
    let phase = arena::match_flow::arena_state(app.world()).map(|s| s.phase);
    assert!(
        flags.iter().any(|(_, at_base, _)| !at_base),
        "tutorial claim: a flag has left its base after 300 frames of the documented command \
         (default warm-up is 3 s, and mode bots do not move until the round). phase={phase:?} flags={flags:?}"
    );
}

/// `ArenaRules::tick` writes [`ArenaConfig::phase_settings`] back onto the session whenever they
/// differ. A rule set whose timer was kept at start (the guide's `ctf_long`) loses it on the
/// next fixed step.
#[test]
fn arena_tick_keeps_a_rule_set_timer() {
    let mut app = tutorial_command_app();
    app.update();
    {
        let mut rules = app.world_mut().resource_mut::<MatchRules>();
        let guide = PhaseSettings {
            time_limit_ticks: Some(36_000),
            on_time_up: TimeUp::Overtime { ticks: Some(3_600) },
            ..PhaseSettings::default()
        };
        rules
            .session
            .set_phases(guide)
            .expect("the guide's phase settings validate");
    }
    app.update();
    let phases = app
        .world()
        .resource::<MatchRules>()
        .session
        .settings()
        .flow
        .phases;
    assert_eq!(
        phases.time_limit_ticks,
        Some(36_000),
        "the next tick replaced the rule set's timer with {:?}",
        phases.time_limit_ticks
    );
    assert_eq!(
        phases.on_time_up,
        TimeUp::Overtime { ticks: Some(3_600) },
        "the next tick replaced the rule set's overtime with {:?}",
        phases.on_time_up
    );
}
