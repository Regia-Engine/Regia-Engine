// SPDX-License-Identifier: MPL-2.0
//! A dedicated server that keeps its world across restarts (ADR-3354): the in-process host
//! (`ServerHost` + `AppHostGame`, a test clock, the local transport, simulated clients over the
//! in-memory link), no sockets, no wall clock.
//!
//! - 100 ticks with an autosave every 60, the host dropped without a shutdown: a new host on the
//!   same directory has the same entity set and component values as at tick 60.
//! - A player who leaves and joins again (also after a restart) gets their kept state back.
//! - A truncated newest save falls back to the previous one with `persistence_save_corrupt`; when
//!   no save decodes, the start fails and the files stay.
//! - The number of autosaves is `ticks / autosave_ticks`; the rotation keeps `keep` saves.
//! - Graceful shutdown, level changes, the console commands and a slot restore.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::rules::{GameRules, GameRulesResource, RulesCtx, RulesPlugin, SpawnRequest};
use regia_framework::save_game::Saveable;
use regia_framework::server_persistence::{
    PersistentPlayer, RegisterPlayerPersistentExt, SaveReason, ServerPersistence, ServerPersistencePlugin,
    level_folder, list_saves, status,
};
use regia_net::NetSessionPlugin;
use regia_net::model::NetPlayer;
use regia_net::server_host::{
    AppHostGame, CliOverrides, ExitCode, HostGame, HostTransport, ServerHost, TestClock,
};
use regia_net::session::lifecycle::{AuthRequestId, AuthResult, PeerAddr, SessionAuthenticator};
use regia_net::session::proto::Hello;
use regia_net::session::{NetSession, SimInput};
use regia_scene::Transform;

/// A level object the save framework keeps.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
#[reflect(Component, Default)]
struct Crate {
    id: u32,
    hp: i32,
}

/// Counts the updates it has seen (every tick changes the world).
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
#[reflect(Component, Default)]
struct Ticker(u64);

/// Per-player state on the `NetPlayer` entity.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
struct Inventory {
    gold: u32,
}

/// Spawns a pawn for every remote player.
struct PawnRules;

impl GameRules for PawnRules {
    fn on_player_joined(&mut self, _ctx: &mut RulesCtx, _player: Entity) -> Option<SpawnRequest> {
        Some(SpawnRequest::default())
    }
}

/// Every joining peer is the platform account in the shared slot.
#[derive(Clone)]
struct FixedId(Arc<Mutex<String>>);

impl SessionAuthenticator for FixedId {
    fn check(&mut self, _: AuthRequestId, _: &Hello, _: &PeerAddr) -> AuthResult {
        AuthResult::Accept {
            platform_id: Some(self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()),
            reserved: false,
        }
    }
}

const LEVEL_A: &str = "levels/a.level.ron";
const LEVEL_B: &str = "levels/b.level.ron";

fn toml(autosave: u32, keep: u32) -> String {
    format!(
        r#"format_version = 2
[server]
name = "Persistent"
max_players = 8
tick_hz = 60
[levels]
rotation = ["{LEVEL_A}", "{LEVEL_B}"]
[persistence]
enabled = true
dir = "saves"
autosave_ticks = {autosave}
keep = {keep}
"#
    )
}

fn write_toml(dir: &Path, text: &str) {
    regia_test_utils::write_atomic(&dir.join("server.toml"), text.as_bytes()).expect("server.toml");
}

/// The level content a fresh level spawns: five crates (ids per level).
fn load_level(app: &mut App, level: &str) -> Result<(), String> {
    let base = if level == LEVEL_B { 100 } else { 0 };
    let world = app.world_mut();
    let old: Vec<Entity> = world
        .query_filtered::<Entity, With<Crate>>()
        .iter(world)
        .collect();
    for e in old {
        world.despawn(e);
    }
    for i in 0..5 {
        world.spawn((
            Saveable,
            Crate {
                id: base + i,
                hp: 100,
            },
            Ticker(0),
        ));
    }
    Ok(())
}

/// Every tick changes every crate; tick 30 builds a new one, tick 40 destroys crate 2.
fn simulate(mut commands: Commands, mut crates: Query<(Entity, &mut Crate, &mut Ticker)>, mut n: Local<u64>) {
    *n += 1;
    for (e, mut c, mut t) in &mut crates {
        t.0 += 1;
        c.hp -= 1;
        if *n == 40 && c.id == 2 {
            commands.entity(e).despawn();
        }
    }
    if *n == 30 {
        commands.spawn((Saveable, Crate { id: 50, hp: 7 }, Ticker(0)));
    }
}

fn advance_session_clock(mut session: ResMut<NetSession>) {
    session.advance_manual_clock(16);
}

struct Server {
    host: ServerHost,
    game: AppHostGame,
    ids: Arc<Mutex<String>>,
}

fn server(dir: &Path) -> Result<Server, (ExitCode, Vec<String>)> {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins((NetSessionPlugin, RulesPlugin, ServerPersistencePlugin));
    app.insert_resource(GameRulesResource {
        rules: Box::new(PawnRules),
    });
    app.register_type::<Crate>().register_type::<Ticker>();
    app.register_player_persistent::<Inventory>()
        .register_player_persistent::<Transform>();
    app.add_systems(First, advance_session_clock);
    app.add_systems(Update, simulate);
    app.world_mut().resource_mut::<NetSession>().set_manual_clock(0);
    let mut game = AppHostGame::new(app);
    game.load_level = Some(Box::new(load_level));
    let clock = TestClock::new();
    let mut host = ServerHost::new(dir, CliOverrides::default(), Box::new(|_| None), Box::new(clock))
        .map_err(|e| (e.exit, e.lines.clone()))?;
    host.start(&mut game, HostTransport::Local)
        .map_err(|e| (e.exit, e.lines.clone()))?;
    let ids = Arc::new(Mutex::new("test:alice".to_owned()));
    game.session()
        .server_mut()
        .expect("hosting")
        .set_authenticator(Box::new(FixedId(ids.clone())));
    Ok(Server { host, game, ids })
}

impl Server {
    fn ticks(&mut self, n: usize) {
        for _ in 0..n {
            assert_eq!(self.host.run_once(&mut self.game), None, "the host stopped");
        }
    }

    fn until(&mut self, limit: usize, what: &str, done: impl Fn(&mut World) -> bool) {
        for _ in 0..limit {
            if done(self.game.app.world_mut()) {
                return;
            }
            self.ticks(1);
        }
        panic!("not reached in {limit} ticks: {what}");
    }

    fn world(&mut self) -> &mut World {
        self.game.app.world_mut()
    }

    fn persistence(&self) -> &ServerPersistence {
        self.game.app.world().resource::<ServerPersistence>()
    }
}

/// The saved world as one sorted list (entity set and values), and its hash.
fn world_state(world: &mut World) -> (Vec<(Crate, Ticker)>, u64) {
    let mut list: Vec<(Crate, Ticker)> = world
        .query_filtered::<(&Crate, &Ticker), With<Saveable>>()
        .iter(world)
        .map(|(c, t)| (c.clone(), t.clone()))
        .collect();
    list.sort();
    // FNV-1a over the debug text: equal lists hash equal, any value change moves it.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in format!("{list:?}").bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (list, h)
}

fn level_dir(dir: &Path, level: &str) -> PathBuf {
    dir.join("saves").join(level_folder(level))
}

fn player_entity(world: &mut World) -> Option<Entity> {
    world
        .query_filtered::<Entity, (With<NetPlayer>, With<PersistentPlayer>)>()
        .iter(world)
        .next()
}

fn pawn_of(world: &World, player: Entity) -> Option<Entity> {
    world
        .get::<regia_framework::net_session_bridge::NetRulesJoined>(player)
        .and_then(|j| j.pawn)
}

#[test]
fn a_dropped_server_comes_back_with_the_world_of_its_last_autosave() {
    let dir = regia_test_utils::unique_temp_dir("server-persist-crash");
    write_toml(&dir, &toml(60, 5));
    let at_60;
    {
        let mut s = server(&dir).expect("start");
        assert_eq!(
            s.persistence().last_restore().and_then(|r| r.sequence),
            None,
            "a fresh server"
        );
        s.ticks(60);
        assert_eq!(s.persistence().counts().autosave, 1, "one autosave at tick 60");
        at_60 = world_state(s.world());
        assert_eq!(
            at_60.0.len(),
            5,
            "crate 2 destroyed, crate 50 built: {:?}",
            at_60.0
        );
        s.ticks(40);
        let at_100 = world_state(s.world());
        assert_ne!(at_100.1, at_60.1, "the world moved on after the autosave");
        assert_eq!(s.persistence().counts().autosave, 1);
        // Dropped without a shutdown: a crash keeps only what was saved.
    }
    let mut s = server(&dir).expect("restart");
    let restored = world_state(s.world());
    assert_eq!(restored.0, at_60.0, "same entity set and values as at tick 60");
    assert_eq!(restored.1, at_60.1, "same state hash");
    let restore = s.persistence().last_restore().cloned().expect("restored");
    assert_eq!(restore.saved_tick, Some(60));
    assert_eq!(restore.entities, 5);
    // The next save continues the sequence.
    s.ticks(60);
    let last = s.persistence().last_save().cloned().expect("saved");
    assert_eq!(last.sequence, 2, "sequences continue across restarts");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_returning_player_gets_their_state_back_also_after_a_restart() {
    let dir = regia_test_utils::unique_temp_dir("server-persist-player");
    write_toml(&dir, &toml(0, 5));
    {
        let mut s = server(&dir).expect("start");
        s.game
            .session()
            .add_simulated_clients(1, SimInput::Idle)
            .expect("client");
        s.until(400, "alice joined", |w| player_entity(w).is_some());
        let player = player_entity(s.world()).expect("player");
        let pawn = pawn_of(s.game.app.world(), player).expect("pawn");
        assert_eq!(
            s.game
                .app
                .world()
                .get::<PersistentPlayer>(player)
                .map(|p| p.identity.clone()),
            Some("test:alice".to_owned())
        );
        s.world().entity_mut(player).insert(Inventory { gold: 42 });
        s.world()
            .entity_mut(pawn)
            .insert(Transform::from_translation(Vec3::new(3.0, 1.0, -7.0)));
        s.game.session().remove_simulated_clients(1).expect("leave");
        s.until(400, "alice left and was saved", |w| {
            w.resource::<ServerPersistence>().counts().player_left == 1
        });
        let record = s
            .persistence()
            .player_record("test:alice")
            .cloned()
            .expect("kept");
        assert!(
            record.player.contains_key("Inventory"),
            "{:?}",
            record.player.keys()
        );
        assert!(record.pawn.contains_key("Transform"), "{:?}", record.pawn.keys());
        assert_eq!(s.persistence().counts().autosave, 0, "autosave off");

        // Back in the same process: a new player entity and a new pawn, the kept state on both.
        s.game
            .session()
            .add_simulated_clients(1, SimInput::Idle)
            .expect("client");
        s.until(400, "alice rejoined", |w| player_entity(w).is_some());
        let player = player_entity(s.world()).expect("player");
        let pawn = pawn_of(s.game.app.world(), player).expect("pawn");
        assert_eq!(
            s.game.app.world().get::<Inventory>(player),
            Some(&Inventory { gold: 42 })
        );
        assert_eq!(
            s.game.app.world().get::<Transform>(pawn).map(|t| t.translation),
            Some(Vec3::new(3.0, 1.0, -7.0))
        );
        // A different account gets nothing of alice's.
        *s.ids.lock().unwrap_or_else(PoisonError::into_inner) = "test:bob".into();
        s.game
            .session()
            .add_simulated_clients(1, SimInput::Idle)
            .expect("client");
        s.until(400, "bob joined", |w| {
            w.query::<&PersistentPlayer>()
                .iter(w)
                .any(|p| p.identity == "test:bob")
        });
        let bob = s
            .world()
            .query::<(Entity, &PersistentPlayer)>()
            .iter(s.game.app.world())
            .find(|(_, p)| p.identity == "test:bob")
            .map(|(e, _)| e)
            .expect("bob");
        assert_eq!(s.game.app.world().get::<Inventory>(bob), None);
        // Alice earns more, then the operator saves; the process is dropped.
        let alice = s
            .world()
            .query::<(Entity, &PersistentPlayer)>()
            .iter(s.game.app.world())
            .find(|(_, p)| p.identity == "test:alice")
            .map(|(e, _)| e)
            .expect("alice");
        s.world().entity_mut(alice).insert(Inventory { gold: 99 });
        let out = s.host.execute(&mut s.game, "save");
        assert!(out.iter().any(|l| l.starts_with("saved ")), "{out:?}");
    }
    // A restart: the live players' state went into the manual save.
    let mut s = server(&dir).expect("restart");
    assert_eq!(
        s.persistence()
            .player_record("test:alice")
            .map(|r| r.name.is_empty()),
        Some(false)
    );
    s.game
        .session()
        .add_simulated_clients(1, SimInput::Idle)
        .expect("client");
    s.until(400, "alice joined after the restart", |w| {
        player_entity(w).is_some()
    });
    let player = player_entity(s.world()).expect("player");
    assert_eq!(
        s.game.app.world().get::<Inventory>(player),
        Some(&Inventory { gold: 99 })
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_truncated_newest_save_falls_back_to_the_previous_one() {
    let dir = regia_test_utils::unique_temp_dir("server-persist-torn");
    write_toml(&dir, &toml(10, 5));
    let (at_20, at_30);
    {
        let mut s = server(&dir).expect("start");
        s.ticks(20);
        at_20 = world_state(s.world());
        s.ticks(10);
        at_30 = world_state(s.world());
        assert_eq!(s.persistence().counts().autosave, 3);
    }
    assert_ne!(at_20.1, at_30.1);
    let saves = list_saves(&level_dir(&dir, LEVEL_A));
    assert_eq!(saves.iter().map(|s| s.sequence).collect::<Vec<_>>(), [3, 2, 1]);
    // A torn newest save: half of it on disk.
    let newest = std::fs::read(&saves[0].path).expect("read");
    std::fs::write(&saves[0].path, &newest[..newest.len() / 2]).expect("truncate");

    let mut s = server(&dir).expect("restart over a torn save");
    assert_eq!(world_state(s.world()), at_20, "the previous save came back");
    let restore = s.persistence().last_restore().cloned().expect("restored");
    assert_eq!(restore.sequence, Some(2));
    assert_eq!(restore.fell_back.len(), 1);
    assert!(
        s.persistence()
            .warnings()
            .iter()
            .any(|w| w.code == "persistence_save_corrupt"),
        "{:?}",
        s.persistence().warnings()
    );
    // The bad save is set aside (kept, out of the rotation); its number is never reused.
    assert!(restore.fell_back[0].to_string_lossy().ends_with(".save.corrupt"));
    assert!(restore.fell_back[0].exists());
    let status = status(s.game.app.world()).expect("status");
    assert_eq!(status.corrupt.len(), 1);
    assert_eq!(status.next_sequence, 4);
    s.ticks(10);
    assert_eq!(s.persistence().last_save().map(|r| r.sequence), Some(4));
    drop(s);

    // Every save bad: the start fails with code 4 and touches nothing.
    for save in list_saves(&level_dir(&dir, LEVEL_A)) {
        std::fs::write(&save.path, b"FRTS").expect("corrupt");
    }
    let before = list_saves(&level_dir(&dir, LEVEL_A));
    match server(&dir) {
        Err((code, lines)) => {
            assert_eq!(code, ExitCode::Content);
            assert!(
                lines.iter().any(|l| l.contains("persistence_no_good_save")),
                "{lines:?}"
            );
        }
        Ok(_) => panic!("a server with no readable save must not start fresh"),
    }
    assert_eq!(
        list_saves(&level_dir(&dir, LEVEL_A)),
        before,
        "the files stay as they were"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_autosave_count_is_a_function_of_ticks_and_the_rotation_keeps_keep() {
    let dir = regia_test_utils::unique_temp_dir("server-persist-count");
    write_toml(&dir, &toml(7, 3));
    let mut s = server(&dir).expect("start");
    for ticks in [1_u64, 6, 7, 13, 14, 50] {
        let now = s.host.tick();
        s.ticks((ticks - now) as usize);
        assert_eq!(
            s.persistence().counts().autosave,
            ticks / 7,
            "after {ticks} ticks"
        );
    }
    let saves = list_saves(&level_dir(&dir, LEVEL_A));
    assert_eq!(
        saves.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        [7, 6, 5],
        "keep = 3"
    );
    let st = status(s.game.app.world()).expect("status");
    assert_eq!(st.next_autosave_tick, Some(56));
    assert_eq!(st.tick, 50);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shutdown_level_changes_console_and_slot_restore() {
    let dir = regia_test_utils::unique_temp_dir("server-persist-ops");
    write_toml(&dir, &toml(0, 5));
    let (at_5, at_stop);
    {
        let mut s = server(&dir).expect("start");
        s.ticks(5);
        at_5 = world_state(s.world());
        let out = s.host.execute(&mut s.game, "save");
        assert!(out[0].starts_with("saved "), "{out:?}");
        s.ticks(5);
        // `restore` needs the confirmation word; an unknown slot is refused.
        let out = s.host.execute(&mut s.game, "restore 1");
        assert!(out[0].contains("confirmation_required"), "{out:?}");
        let out = s.host.execute(&mut s.game, "restore 9 confirm");
        assert!(out[0].contains("persistence_slot_unknown"), "{out:?}");
        let out = s.host.execute(&mut s.game, "restore 1 confirm");
        assert!(out[0].starts_with("restored slot 1"), "{out:?}");
        assert_eq!(world_state(s.world()), at_5, "rolled back to slot 1");
        let out = s.host.execute(&mut s.game, "persistence");
        assert!(out[0].contains("manual=1"), "{out:?}");
        assert!(
            s.host
                .execute(&mut s.game, "help")
                .iter()
                .any(|l| l.starts_with("save "))
        );

        // A level change saves the level being left; the new level starts fresh, and coming back
        // restores the first level's world.
        s.ticks(3);
        let a_before = world_state(s.world());
        s.host.change_level(&mut s.game, LEVEL_B).expect("travel");
        assert_eq!(s.persistence().counts().level_change, 1);
        assert!(
            world_state(s.world()).0.iter().all(|(c, _)| c.id >= 100),
            "level b content"
        );
        s.ticks(2);
        s.host.change_level(&mut s.game, LEVEL_A).expect("travel back");
        assert_eq!(s.persistence().counts().level_change, 2);
        assert_eq!(world_state(s.world()), a_before, "level a as it was left");
        assert_eq!(list_saves(&level_dir(&dir, LEVEL_B)).len(), 1);

        // A graceful shutdown saves after the drain.
        s.ticks(4);
        s.host.begin_drain(&mut s.game, 0.0);
        let mut code = None;
        for _ in 0..20 {
            if let Some(c) = s.host.run_once(&mut s.game) {
                code = Some(c);
                break;
            }
        }
        assert_eq!(code, Some(ExitCode::Ok));
        let last = s.persistence().last_save().cloned().expect("saved");
        assert_eq!(last.reason, SaveReason::Shutdown);
        assert_eq!(s.persistence().counts().shutdown, 1);
        at_stop = world_state(s.world());
    }
    let mut s = server(&dir).expect("restart");
    let restore = s.persistence().last_restore().cloned().expect("restored");
    assert_eq!(restore.level, LEVEL_A);
    assert_eq!(restore.sequence, Some(4), "manual, two level changes, shutdown");
    assert_eq!(world_state(s.world()), at_stop, "the world as the drain left it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn persistence_off_writes_nothing_and_v1_files_still_start() {
    let dir = regia_test_utils::unique_temp_dir("server-persist-off");
    write_toml(
        &dir,
        &format!("format_version = 1\n[levels]\nrotation = [\"{LEVEL_A}\"]\n"),
    );
    let mut s = server(&dir).expect("a v1 server.toml starts");
    s.ticks(30);
    assert!(!s.persistence().active());
    assert!(!dir.join("saves").exists(), "nothing written");
    let out = s.host.execute(&mut s.game, "save");
    assert!(out[0].contains("persistence_disabled"), "{out:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
