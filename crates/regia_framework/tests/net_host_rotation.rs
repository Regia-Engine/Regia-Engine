// SPDX-License-Identifier: MPL-2.0
//! A dedicated host ends a match from `GameRules` or `MatchRules` and rotates
//! (`book/agents/net-session-players.md`, ADR-8345). The host clock is a `TestClock`.

use std::time::Duration;

use regia_app::prelude::*;
use regia_framework::rules::{
    GameRules, GameRulesResource, MatchOptions, MatchRules, RulesCtx, RulesPlugin, SpawnRequest,
};
use regia_net::server_host::{
    AppHostGame, CliOverrides, HostEvent, HostGame, HostGameHooks, HostTransport, ServerHost, TestClock,
};
use regia_net::{NetSession, NetSessionPlugin, SessionRules};
use regia_scene::level::LevelSettings;
use regia_test_utils::TempDir;

const LEVEL_A: &str = "levels/a.level.ron";
const LEVEL_B: &str = "levels/b.level.ron";

/// The host tick the rules last saw.
#[derive(Resource, Default)]
struct SeenTick(u64);

/// How many times `on_level_start` ran.
#[derive(Resource, Default)]
struct LevelStarts(u32);

/// `is_match_over` becomes true on the first host tick.
struct TickRules;

impl GameRules for TickRules {
    fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
        None
    }

    fn on_level_start(&mut self, ctx: &mut RulesCtx) {
        ctx.world.resource_mut::<LevelStarts>().0 += 1;
    }

    fn is_match_over(&self, world: &World) -> bool {
        world.get_resource::<SeenTick>().is_some_and(|tick| tick.0 >= 1)
    }
}

/// Match end is always true. Used to prove an explicit `SessionRules` is left in place.
struct AlwaysOver;

impl GameRules for AlwaysOver {
    fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
        None
    }

    fn is_match_over(&self, _world: &World) -> bool {
        true
    }
}

/// Installed with `set_rules` before the bridge. Idle slowdown is the observable that it stayed.
#[derive(Clone, Copy)]
struct KeepRules;

impl SessionRules for KeepRules {
    fn allow_idle_slowdown(&self) -> bool {
        true
    }
}

fn note_tick(world: &mut World, event: &HostEvent<'_>) -> Result<(), String> {
    if let HostEvent::Ticked { tick } = *event {
        world.insert_resource(SeenTick(tick));
    }
    Ok(())
}

fn reset_level_settings(app: &mut App, _level: &str) -> Result<(), String> {
    app.insert_resource(LevelSettings::default());
    Ok(())
}

struct Running {
    _dir: TempDir,
    host: ServerHost,
    game: AppHostGame,
    clock: TestClock,
}

fn server_text(delay_s: f64, levels_body: &str) -> String {
    format!(
        r#"format_version = 4
[server]
name = "Rotation"
tick_hz = 60
idle_tick_hz = 60
[levels]
{levels_body}
match_end_delay_s = {delay_s}
[status]
enabled = false
"#
    )
}

fn boot(dir: TempDir, reset_level: bool, setup: impl FnOnce(&mut App)) -> Running {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    setup(&mut app);
    app.add_plugins((NetSessionPlugin, RulesPlugin));
    app.world_mut().resource_mut::<FixedTime>().max_substeps = 1;
    app.init_resource::<LevelStarts>();
    app.world_mut()
        .resource_mut::<HostGameHooks>()
        .add_event_hook(note_tick);
    let mut game = AppHostGame::new(app);
    if reset_level {
        game.load_level = Some(Box::new(reset_level_settings));
    }
    let clock = TestClock::new();
    let mut host = ServerHost::new(
        dir.path(),
        CliOverrides::default(),
        Box::new(|_| None),
        Box::new(clock.clone()),
    )
    .expect("config");
    host.start(&mut game, HostTransport::Local).expect("start");
    Running {
        _dir: dir,
        host,
        game,
        clock,
    }
}

fn write_server(dir: &TempDir, text: &str) {
    regia_test_utils::write_atomic(&dir.join("server.toml"), text.as_bytes()).expect("server.toml");
}

/// One host tick must not rotate. After `match_end_delay_s` of the fake clock, the next tick does.
fn rotate_after_delay(running: &mut Running) {
    assert!(running.host.run_once(&mut running.game).is_none());
    assert_eq!(running.host.level(), LEVEL_A, "the delay holds the current level");
    running.clock.advance(Duration::from_secs(1));
    assert!(running.host.run_once(&mut running.game).is_none());
}

#[test]
fn a_dedicated_host_running_game_rules_rotates_when_the_rules_end_the_match() {
    let dir = TempDir::new("game-rules-rotate");
    write_server(
        &dir,
        &server_text(1.0, &format!("rotation = [\"{LEVEL_A}\", \"{LEVEL_B}\"]")),
    );
    let mut running = boot(dir, false, |app| {
        app.insert_resource(GameRulesResource {
            rules: Box::new(TickRules),
        });
    });
    rotate_after_delay(&mut running);
    assert_eq!(running.host.level(), LEVEL_B, "row 2 after the delay");
    assert_eq!(
        running.game.app.world().resource::<LevelStarts>().0,
        0,
        "travel finishes on the following tick"
    );
    assert!(running.host.run_once(&mut running.game).is_none());
    assert_eq!(
        running.host.level(),
        LEVEL_B,
        "the following tick does not rotate again"
    );
    assert_eq!(
        running.game.app.world().resource::<LevelStarts>().0,
        1,
        "on_travel_finished calls on_level_start"
    );
}

#[test]
fn a_match_rules_board_ending_the_match_rotates_the_host() {
    let dir = TempDir::new("match-board-rotate");
    write_server(
        &dir,
        &server_text(1.0, &format!("rotation = [\"{LEVEL_A}\", \"{LEVEL_B}\"]")),
    );
    let mut running = boot(dir, false, |app| {
        let mut rules = MatchRules::default();
        rules.session.board.match_over = true;
        app.insert_resource(rules);
    });
    rotate_after_delay(&mut running);
    assert_eq!(running.host.level(), LEVEL_B);
}

#[test]
fn an_explicit_session_rules_is_not_replaced() {
    let dir = TempDir::new("explicit-rules");
    write_server(
        &dir,
        &server_text(0.0, &format!("rotation = [\"{LEVEL_A}\", \"{LEVEL_B}\"]")),
    );
    let mut running = boot(dir, false, |app| {
        app.insert_resource(NetSession::default());
        app.world_mut().resource_mut::<NetSession>().set_rules(KeepRules);
        app.insert_resource(GameRulesResource {
            rules: Box::new(AlwaysOver),
        });
    });
    assert!(running.game.session().has_explicit_rules());
    assert!(HostGame::allow_idle_slowdown(&mut running.game));
    assert!(running.host.run_once(&mut running.game).is_none());
    assert!(running.host.run_once(&mut running.game).is_none());
    assert_eq!(
        running.host.level(),
        LEVEL_A,
        "the bridge left the explicit rules"
    );
    assert!(running.game.session().has_explicit_rules());
    assert!(HostGame::allow_idle_slowdown(&mut running.game));
}

#[test]
fn playlist_rows_set_the_mode_and_options_of_the_next_level() {
    let dir = TempDir::new("playlist-rows");
    let playlist = r#"(
    format_version: 1,
    looping: true,
    entries: [
        (map_id: "levels/a.level.ron", mode: "", options: {}),
        (map_id: "levels/b.level.ron", mode: "ctf", options: { "limit": "10" }),
    ],
)
"#;
    regia_test_utils::write_atomic(&dir.join("rows.playlist.ron"), playlist.as_bytes()).expect("playlist");
    write_server(&dir, &server_text(1.0, "playlist = \"rows.playlist.ron\""));
    let mut running = boot(dir, true, |app| {
        let mut rules = MatchRules::default();
        rules.session.board.match_over = true;
        app.insert_resource(rules);
    });
    rotate_after_delay(&mut running);
    assert_eq!(running.host.level(), LEVEL_B);
    let mode = running
        .game
        .app
        .world()
        .resource::<LevelSettings>()
        .game_rules
        .clone();
    let limit = running
        .game
        .app
        .world()
        .resource::<MatchOptions>()
        .options
        .get("limit")
        .cloned();
    assert_eq!(mode.as_deref(), Some("ctf"));
    assert_eq!(limit.as_deref(), Some("10"));
}
