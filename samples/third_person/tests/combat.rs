// SPDX-License-Identifier: MIT-0
//! Headless combat tests for the third-person demo (ADR-2596): fixed 16 ms frames, scripted input,
//! no window and no renderer, a temporary data directory per test.
//!
//! - an enemy notices the player (perception) and reaches it (behavior tree + nav crowd);
//! - scripted attacks (the montage combo) kill a grunt and a ragdoll appears;
//! - the player dies to an enemy and respawns at the last checkpoint;
//! - settings and progress survive save, quit and load (a second app on the same directory);
//! - a save from before combat (shape v1) loads and migrates;
//! - the title screen holds play until Play, and the procedural sounds build and play.

use std::path::{Path, PathBuf};

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::character::CharacterState;
use regia_framework::flow::GameFlow;
use regia_framework::prelude::GameFlowSettings;
use regia_framework::settings::{GameSettings, GraphicsPreset};
use regia_scene::Transform;
use third_person::checkpoint::CombatProgress;
use third_person::combat::{self, CombatStats};
use third_person::enemy::{Enemy, EnemyBrain, EnemyKind};
use third_person::menu::{self, MenuAction};
use third_person::{SampleDataDir, ThirdPersonSample, collectible};

/// A headless app on `dir`; `title` keeps the title screen (otherwise play starts at once).
fn app_in(dir: &Path, title: bool) -> App {
    let mut app = App::new();
    app.insert_resource(SampleDataDir(dir.to_path_buf()));
    if !title {
        app.insert_resource(GameFlowSettings {
            skip_main_menu: true,
            ..Default::default()
        });
    }
    app.add_plugins((MinimalPlugins, ThirdPersonSample))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    app.update();
    app
}

fn temp(tag: &str) -> PathBuf {
    regia_test_utils::unique_temp_dir(tag)
}

fn run(app: &mut App, frames: u32) {
    for _ in 0..frames {
        app.update();
    }
}

/// Runs until `done` holds, at most `max` frames; returns the frame it held on.
fn run_until(app: &mut App, max: u32, mut done: impl FnMut(&mut App) -> bool) -> Option<u32> {
    for frame in 1..=max {
        app.update();
        if done(app) {
            return Some(frame);
        }
    }
    None
}

fn player(app: &App) -> Entity {
    third_person::input::possessed(app.world(), 0).expect("player 0 possesses a character")
}

fn enemy(app: &mut App, id: u32) -> Option<Entity> {
    app.world_mut()
        .query::<(Entity, &Enemy)>()
        .iter(app.world())
        .find(|(_, e)| e.id == id)
        .map(|(e, _)| e)
}

fn position(app: &App, entity: Entity) -> Vec3 {
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

/// Puts the player capsule at rest at `at` (capsule center).
fn place(app: &mut App, at: Vec3) {
    let character = player(app);
    if let Some(mut t) = app.world_mut().get_mut::<Transform>(character) {
        t.translation = at;
    }
    if let Some(mut s) = app.world_mut().get_mut::<CharacterState>(character) {
        s.velocity = Vec3::ZERO;
    }
}

fn health(app: &App, entity: Entity) -> f32 {
    combat::health(app.world(), entity).unwrap_or(-1.0)
}

fn stats(app: &App) -> CombatStats {
    app.world().resource::<CombatStats>().clone()
}

#[test]
fn combat_data_trees_and_navmesh_load_clean() {
    let dir = temp("tp-data");
    let mut app = app_in(&dir, false);
    run(&mut app, 5);
    let diags = app.world().resource::<combat::CombatDiagnostics>();
    assert!(diags.errors.is_empty(), "combat data: {:?}", diags.errors);
    let trees = app.world().resource::<third_person::enemy::EnemyTrees>();
    assert!(trees.errors.is_empty(), "trees: {:?}", trees.errors);
    assert!(trees.grunt.is_some() && trees.ranged.is_some());
    assert!(
        app.world().contains_resource::<regia_nav::NavMesh>(),
        "navmesh baked"
    );
    let kinds: Vec<EnemyKind> = app
        .world_mut()
        .query::<&Enemy>()
        .iter(app.world())
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds.len(), 3, "three enemies spawned: {kinds:?}");
    assert!(kinds.contains(&EnemyKind::Grunt) && kinds.contains(&EnemyKind::Ranged));
    // Everyone has health from the attribute files.
    let grunt = enemy(&mut app, 0).expect("grunt");
    assert_eq!(health(&app, grunt), 40.0);
    assert_eq!(health(&app, player(&app)), 100.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_patrolling_grunt_notices_the_player_and_reaches_it() {
    let dir = temp("tp-notice");
    let mut app = app_in(&dir, false);
    run(&mut app, 30);
    let grunt = enemy(&mut app, 0).expect("grunt");
    // It patrols: it moves along its route before it sees anyone.
    let start = position(&app, grunt);
    run(&mut app, 90);
    let walked = position(&app, grunt) - start;
    assert!(
        Vec3::new(walked.x, 0.0, walked.z).length() > 1.0,
        "patrol moved the grunt: {walked:?}"
    );
    assert_eq!(
        app.world()
            .get::<EnemyBrain>(grunt)
            .map(|b| b.state.clone())
            .as_deref(),
        Some("patrol")
    );

    // Stand 6 m in front of it (inside its sight cone, nothing in between).
    let at = position(&app, grunt);
    let forward = app.world().get::<Enemy>(grunt).expect("enemy").forward();
    place(
        &mut app,
        Vec3::new(at.x + forward.x * 6.0, 1.05, at.z + forward.z * 6.0),
    );
    let noticed = run_until(&mut app, 120, |app| {
        app.world().get::<EnemyBrain>(grunt).is_some_and(|b| b.alert)
    });
    assert!(noticed.is_some(), "the grunt noticed the player");
    let brain = app.world().get::<EnemyBrain>(grunt).cloned().expect("brain");
    assert_eq!(brain.noticed_by, "sight", "{brain:?}");
    assert!(brain.noticed >= 1);

    // It chases and reaches attack range, then strikes.
    let me = player(&app);
    let reached = run_until(&mut app, 600, |app| {
        let d = position(app, grunt) - position(app, me);
        Vec3::new(d.x, 0.0, d.z).length() <= 1.9
    });
    assert!(reached.is_some(), "the grunt reached the player");
    let struck = run_until(&mut app, 240, |app| stats(app).enemy_hits > 0);
    assert!(struck.is_some(), "the grunt hit the player");
    // `current` is recomputed on the next fixed step after the hit's base edit.
    run(&mut app, 3);
    assert!(
        health(&app, me) < 100.0,
        "the hit took health through the hit effect"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_grunt_gives_up_when_the_player_leaves() {
    let dir = temp("tp-giveup");
    let mut app = app_in(&dir, false);
    run(&mut app, 30);
    let grunt = enemy(&mut app, 0).expect("grunt");
    let at = position(&app, grunt);
    let forward = app.world().get::<Enemy>(grunt).expect("enemy").forward();
    place(
        &mut app,
        Vec3::new(at.x + forward.x * 5.0, 1.05, at.z + forward.z * 5.0),
    );
    assert!(
        run_until(&mut app, 120, |app| app
            .world()
            .get::<EnemyBrain>(grunt)
            .is_some_and(|b| b.alert))
        .is_some(),
        "noticed"
    );
    // Teleport far away, behind a wall of distance: out of sight, out of earshot, past the leash.
    place(&mut app, Vec3::new(24.0, 2.0, 0.0));
    let gave_up = run_until(&mut app, 900, |app| {
        app.world()
            .get::<EnemyBrain>(grunt)
            .is_some_and(|b| b.gave_up >= 1 && !b.alert)
    });
    assert!(
        gave_up.is_some(),
        "the grunt gave up: {:?}",
        app.world().get::<EnemyBrain>(grunt)
    );
    run(&mut app, 30);
    assert_eq!(
        app.world()
            .get::<EnemyBrain>(grunt)
            .map(|b| b.state.clone())
            .as_deref(),
        Some("patrol"),
        "back on patrol"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scripted_combo_kills_a_grunt_and_a_ragdoll_appears() {
    let dir = temp("tp-kill");
    let mut app = app_in(&dir, false);
    run(&mut app, 30);
    let grunt = enemy(&mut app, 0).expect("grunt");
    let at = position(&app, grunt);
    let forward = app.world().get::<Enemy>(grunt).expect("enemy").forward();
    place(
        &mut app,
        Vec3::new(at.x + forward.x * 1.4, 1.05, at.z + forward.z * 1.4),
    );
    let me = player(&app);
    // Press attack every 12 frames (0.19 s): each press inside a combo window links the next
    // swing, so this plays full three-hit combos.
    let mut frame = 0u32;
    let killed = run_until(&mut app, 900, |app| {
        frame += 1;
        if frame.is_multiple_of(12) {
            third_person::melee::press_attack(app.world_mut(), me);
        }
        stats(app).enemies_killed >= 1
    });
    assert!(killed.is_some(), "the grunt died; stats {:?}", stats(&app));
    let melee = app
        .world()
        .get::<third_person::melee::MeleeState>(me)
        .cloned()
        .expect("melee");
    assert!(melee.swings >= 3, "combo swings: {melee:?}");
    assert!(melee.hits >= 2, "hits landed: {melee:?}");
    assert!(app.world().get_entity(grunt).is_err(), "the grunt entity is gone");
    assert_eq!(app.world().resource::<CombatProgress>().defeated, vec![0]);

    // A limp ragdoll lies where it fell, its bodies built and dressed.
    let built = run_until(&mut app, 30, |app| {
        app.world_mut()
            .query::<&third_person::ragdoll_fx::Corpse>()
            .iter(app.world())
            .any(|c| c.team == combat::TEAM_ENEMY && c.bodies >= 8)
    });
    assert!(built.is_some(), "an enemy corpse with ragdoll bodies");
    // The HUD showed damage numbers and the hits made sound.
    let numbers: u32 = app
        .world_mut()
        .query::<&third_person::hud::DamageNumbers>()
        .iter(app.world())
        .map(|d| d.total)
        .sum();
    assert!(numbers >= 2, "damage numbers shown: {numbers}");
    assert!(app.world().resource::<third_person::sound::SoundStats>().hits >= 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_player_dies_to_a_grunt_and_respawns_at_the_checkpoint() {
    let dir = temp("tp-respawn");
    let mut app = app_in(&dir, false);
    run(&mut app, 30);
    // Step on checkpoint 1.
    let pad = third_person::checkpoint::PADS[1];
    place(&mut app, pad + Vec3::new(0.0, 0.85, 0.0));
    assert!(
        run_until(&mut app, 30, |app| app
            .world()
            .resource::<CombatProgress>()
            .checkpoint
            == 1)
        .is_some(),
        "checkpoint 1 active"
    );
    // Walk into the grunt and let it beat the player down (no input: the player stands).
    let grunt = enemy(&mut app, 0).expect("grunt");
    let at = position(&app, grunt);
    let forward = app.world().get::<Enemy>(grunt).expect("enemy").forward();
    place(
        &mut app,
        Vec3::new(at.x + forward.x * 2.0, 1.05, at.z + forward.z * 2.0),
    );
    let me = player(&app);
    let died = run_until(&mut app, 2400, |app| stats(app).player_deaths >= 1);
    assert!(
        died.is_some(),
        "the player died: health {} stats {:?}",
        health(&app, me),
        stats(&app)
    );
    assert!(app.world().get::<combat::Downed>(me).is_some(), "downed");
    assert!(
        app.world_mut()
            .query::<&third_person::ragdoll_fx::Corpse>()
            .iter(app.world())
            .any(|c| c.team == combat::TEAM_PLAYER),
        "the player fell as a ragdoll"
    );
    let respawned = run_until(&mut app, 240, |app| stats(app).respawns >= 1);
    assert!(respawned.is_some(), "respawned");
    run(&mut app, 3);
    let spot = third_person::checkpoint::respawn_point(app.world());
    assert!(
        (stats(&app).last_respawn - spot).length() < 1e-3,
        "at the checkpoint"
    );
    let here = position(&app, me);
    assert!(
        Vec3::new(here.x - pad.x, 0.0, here.z - pad.z).length() < 1.5,
        "the character stands at checkpoint 1: {here:?}"
    );
    assert_eq!(health(&app, me), 100.0, "full health again");
    assert!(app.world().get::<combat::Downed>(me).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_and_progress_survive_save_quit_and_load() {
    let dir = temp("tp-persist");
    {
        let mut app = app_in(&dir, false);
        run(&mut app, 10);
        for action in [
            MenuAction::Master(false),
            MenuAction::Master(false),
            MenuAction::Master(false),
            MenuAction::Music(false),
            MenuAction::Effects(false),
            MenuAction::Sensitivity(true),
            MenuAction::Sensitivity(true),
            MenuAction::InvertY,
            MenuAction::Preset(true),
        ] {
            menu::apply_action(app.world_mut(), action);
        }
        run(&mut app, 3);
        // Progress: a coin, checkpoint 1, the caster defeated.
        collectible::collect(app.world_mut(), 0);
        let pad = third_person::checkpoint::PADS[1];
        place(&mut app, pad + Vec3::new(0.0, 0.85, 0.0));
        run(&mut app, 10);
        let caster = enemy(&mut app, 1).expect("caster");
        let me = player(&app);
        assert!(combat::deal_damage(app.world_mut(), me, caster, 500.0));
        assert!(
            run_until(&mut app, 30, |app| stats(app).enemies_killed == 1).is_some(),
            "caster died"
        );
        third_person::save::save_now(app.world_mut());
        assert!(
            run_until(&mut app, 10, |app| app
                .world()
                .resource::<third_person::save::SaveStatus>()
                .saves
                == 1)
            .is_some(),
            "saved: {:?}",
            app.world().resource::<third_person::save::SaveStatus>()
        );
        // Quit: dropping the app flushes the settings file.
    }
    assert!(dir.join("settings.ron").is_file(), "settings persisted");
    assert!(dir.join("saves/Chapter1.save").is_file(), "save slot written");

    // Relaunch: the title screen, settings already loaded.
    let mut app = app_in(&dir, true);
    run(&mut app, 3);
    let settings = app.world().resource::<GameSettings>().clone();
    assert!((settings.audio.master - 0.7).abs() < 1e-4, "{:?}", settings.audio);
    assert!((settings.audio.music - 0.9).abs() < 1e-4);
    assert!((settings.audio.sfx - 0.9).abs() < 1e-4);
    assert!(
        (settings.input.sensitivity - 1.2).abs() < 1e-4,
        "{:?}",
        settings.input
    );
    assert!(settings.input.invert_y);
    assert_eq!(settings.graphics.preset, GraphicsPreset::High);
    assert_eq!(
        *app.world().resource::<State<GameFlow>>().get(),
        GameFlow::MainMenu
    );
    // Continue: load and play.
    menu::apply_action(app.world_mut(), MenuAction::Continue);
    assert!(
        run_until(&mut app, 10, |app| app
            .world()
            .resource::<third_person::save::SaveStatus>()
            .loads
            == 1)
        .is_some(),
        "loaded: {:?}",
        app.world().resource::<third_person::save::SaveStatus>()
    );
    run(&mut app, 3);
    assert_eq!(
        *app.world().resource::<State<GameFlow>>().get(),
        GameFlow::Playing
    );
    let progress = app.world().resource::<CombatProgress>().clone();
    assert_eq!(progress.checkpoint, 1);
    assert_eq!(progress.defeated, vec![1]);
    assert!(enemy(&mut app, 1).is_none(), "the defeated caster stays defeated");
    assert!(enemy(&mut app, 0).is_some(), "the grunt is still there");
    assert_eq!(
        app.world().resource::<collectible::CollectProgress>().collected,
        vec![0]
    );
    assert_eq!(
        app.world()
            .resource::<third_person::save::SaveStatus>()
            .loaded_shape,
        2
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_save_from_before_combat_loads_and_migrates() {
    let dir = temp("tp-migrate");
    // Write a v1 save the way the sample did before combat: `SaveGame::save` with only the
    // collectible progress registered.
    {
        use regia_framework::prelude::{RegisterSaveableExt, SaveDirectory, SaveGame, SavePlugin, SaveSpec};
        let mut old = App::new();
        old.add_plugins((MinimalPlugins, SavePlugin::default()))
            .register_saveable_resource::<collectible::CollectProgress>()
            .insert_resource(collectible::CollectProgress {
                collected: vec![2, 3],
            });
        old.update();
        let slot = SaveGame::slot(&SaveDirectory(dir.join("saves")), "Chapter1");
        SaveGame::save(&slot, old.world_mut(), &SaveSpec::default()).expect("v1 save written");
    }
    let mut app = app_in(&dir, false);
    run(&mut app, 5);
    // Play a little: a checkpoint and a kill the v1 file knows nothing about.
    app.world_mut().insert_resource(CombatProgress {
        checkpoint: 2,
        defeated: vec![0],
    });
    run(&mut app, 3);
    assert!(enemy(&mut app, 0).is_none());
    third_person::save::load_now(app.world_mut());
    assert!(
        run_until(&mut app, 10, |app| app
            .world()
            .resource::<third_person::save::SaveStatus>()
            .loads
            == 1)
        .is_some(),
        "loaded: {:?}",
        app.world().resource::<third_person::save::SaveStatus>()
    );
    run(&mut app, 3);
    let status = app.world().resource::<third_person::save::SaveStatus>().clone();
    assert_eq!(status.loaded_shape, 1, "{status:?}");
    assert_eq!(status.migrated, 1);
    assert_eq!(
        app.world().resource::<CombatProgress>(),
        &CombatProgress::default()
    );
    assert_eq!(
        app.world().resource::<collectible::CollectProgress>().collected,
        vec![2, 3]
    );
    assert!(
        enemy(&mut app, 0).is_some(),
        "v1 had no defeated enemies: the grunt is back"
    );
    assert_eq!(
        third_person::save::loaded_shape(app.world()),
        2,
        "stamped as v2 now"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_title_screen_holds_play_until_play() {
    let dir = temp("tp-title");
    let mut app = app_in(&dir, true);
    run(&mut app, 20);
    assert_eq!(
        *app.world().resource::<State<GameFlow>>().get(),
        GameFlow::MainMenu
    );
    assert_eq!(app.world().resource::<menu::MenuState>().screen, "title");
    // Input does nothing behind the title; the enemies hold.
    let start = position(&app, player(&app));
    app.world_mut()
        .resource_mut::<third_person::input::ActionKeys>()
        .held
        .insert("KeyW".into());
    run(&mut app, 30);
    let moved = position(&app, player(&app)) - start;
    assert!(
        Vec3::new(moved.x, 0.0, moved.z).length() < 0.05,
        "no movement on the title: {moved:?}"
    );
    let grunt = enemy(&mut app, 0).expect("grunt");
    assert_eq!(
        app.world()
            .get::<EnemyBrain>(grunt)
            .map(|b| b.state.clone())
            .as_deref(),
        Some("hold")
    );
    // Settings opens over the title and closes back to it.
    menu::apply_action(app.world_mut(), MenuAction::OpenSettings);
    run(&mut app, 1);
    assert_eq!(app.world().resource::<menu::MenuState>().screen, "settings");
    menu::apply_action(app.world_mut(), MenuAction::CloseSettings);
    run(&mut app, 1);
    assert_eq!(app.world().resource::<menu::MenuState>().screen, "title");
    // Play.
    menu::apply_action(app.world_mut(), MenuAction::Play);
    assert!(
        run_until(&mut app, 5, |app| *app
            .world()
            .resource::<State<GameFlow>>()
            .get()
            == GameFlow::Playing)
        .is_some(),
        "playing"
    );
    run(&mut app, 30);
    let moved = position(&app, player(&app)) - start;
    assert!(
        Vec3::new(moved.x, 0.0, moved.z).length() > 0.5,
        "W walks once playing: {moved:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn procedural_sounds_build_and_footsteps_play() {
    let dir = temp("tp-sound");
    let mut app = app_in(&dir, false);
    run(&mut app, 30);
    let sound = app.world().resource::<third_person::sound::SoundStats>().clone();
    assert!(sound.errors.is_empty(), "{:?}", sound.errors);
    assert!(sound.ambient_playing && sound.music_playing, "{sound:?}");
    assert_eq!(sound.ambient_frames, 6 * 48_000, "a 6 s loop at 48 kHz");
    // Both beds were started on the audio device (headless backend): two voices accepted.
    assert!(app.world().resource::<regia_audio::Audio>().played_count() >= 2);
    app.world_mut()
        .resource_mut::<third_person::input::ActionKeys>()
        .held
        .insert("KeyD".into());
    run(&mut app, 120);
    let sound = app.world().resource::<third_person::sound::SoundStats>().clone();
    assert!(sound.footsteps >= 3, "footsteps while walking: {sound:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_grunt_hears_footsteps_behind_it() {
    let dir = temp("tp-hear");
    let mut app = app_in(&dir, false);
    run(&mut app, 30);
    let grunt = enemy(&mut app, 0).expect("grunt");
    let at = position(&app, grunt);
    let forward = app.world().get::<Enemy>(grunt).expect("enemy").forward();
    // 3 m behind it: outside the sight cone, inside its hearing (7 m) and a step's reach (5 m).
    place(
        &mut app,
        Vec3::new(at.x - forward.x * 3.0, 1.05, at.z - forward.z * 3.0),
    );
    run(&mut app, 2);
    assert!(
        !app.world().get::<EnemyBrain>(grunt).is_some_and(|b| b.alert),
        "standing still behind it is not noticed"
    );
    // Walk a few steps (camera-relative D = +X).
    app.world_mut()
        .resource_mut::<third_person::input::ActionKeys>()
        .held
        .insert("KeyD".into());
    let heard = run_until(&mut app, 60, |app| {
        app.world().get::<EnemyBrain>(grunt).is_some_and(|b| b.alert)
    });
    assert!(
        heard.is_some(),
        "footsteps alerted the grunt: {:?}",
        app.world().get::<EnemyBrain>(grunt)
    );
    let brain = app.world().get::<EnemyBrain>(grunt).cloned().expect("brain");
    assert_eq!(brain.noticed_by, "hearing", "{brain:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
