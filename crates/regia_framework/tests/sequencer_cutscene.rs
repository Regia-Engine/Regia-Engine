// SPDX-License-Identifier: MPL-2.0
//! Spawnable bindings and the cutscene flow (ADR-2948): fixed steps, no wall clock, no GPU.

use std::path::{Path, PathBuf};
use std::time::Duration;

use regia_app::prelude::*;
use regia_core::id::Guid;
use regia_core::math::Vec3;
use regia_framework::sequencer::{
    Binding, CUTSCENE_CONTEXT, Channel, CutsceneEnded, CutscenePhase, CutsceneSettings, EndPolicy,
    MarkerReached, NetCutscenes, PlaybackStatus, SKIP_ACTION, Sequence, SequenceBindings, SequencePlayer,
    SequenceSpawnables, SequencerPlugin, SkipSettings, Spawnable, cutscene_state, skip_cutscene,
    start_cutscene, stop_cutscene,
};
use regia_input::Binding as InputBinding;
use regia_input::prelude::*;
use regia_platform::{ButtonState, KeyboardInput};
use regia_scene::Transform;
use regia_scene::components::EntityGuid;

const STEP: Duration = Duration::from_millis(100);

/// A project with `actors/hero.prefab.ron` (a root and a child) and `actors/prop.prefab.ron`.
fn project() -> PathBuf {
    let dir = regia_test_utils::unique_temp_dir("seq-spawnables");
    std::fs::create_dir_all(dir.join("actors")).expect("temp dir");
    let v = regia_scene::format::SCENE_FORMAT_VERSION;
    let prefab = |root: &str, child: &str| {
        format!(
            r#"SceneFile(format_version: {v}, entities: [
    (guid: "{root}", name: "Root", components: {{
        "regia_scene::transform::Transform": (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
    }}),
    (guid: "{child}", name: "Child", parent: "{root}", components: {{
        "regia_scene::transform::Transform": (translation: (0.0, 1.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
    }}),
])
"#
        )
    };
    write(
        &dir.join("actors/hero.prefab.ron"),
        &prefab(
            "44fe6f0a-1e42-4c11-8d5a-9b3c2f7e10d0",
            "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d",
        ),
    );
    write(
        &dir.join("actors/prop.prefab.ron"),
        &prefab(
            "0f1e8a44-90d3-4f2e-b7c5-6a2d0e4f8b11",
            "5b6c7d8e-9f0a-4b1c-8d2e-3f4a5b6c7d8e",
        ),
    );
    dir
}

fn write(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).expect("write");
    std::fs::rename(&tmp, path).expect("rename");
}

/// A 3 s sequence: `hero` spawned over [0.5, 1.5) and moved, `prop` from 2.0 to the end (kept on
/// stop), `door` (a hand-made binding) moved over the whole length, and five markers.
fn cinematic() -> Sequence {
    Sequence::from_ron(
        r#"(version: 5, name: "intro", tracks: [
            (name: "hero_move", target: "hero", property: "transform.position",
                keys: Vec3([(time: 0.5, value: (0.0, 0.0, 0.0)), (time: 1.5, value: (5.0, 0.0, 0.0))])),
            (name: "prop_move", target: "prop", property: "transform.position",
                keys: Vec3([(time: 2.0, value: (1.0, 0.0, 0.0)), (time: 3.0, value: (1.0, 2.0, 0.0))])),
            (name: "door_move", target: "door", property: "transform.position",
                keys: Vec3([(time: 0.0, value: (0.0, 0.0, 0.0)), (time: 3.0, value: (0.0, 0.0, 9.0))])),
        ], markers: [
            (name: "a", time: 0.0), (name: "b", time: 0.75), (name: "c", time: 1.5),
            (name: "d", time: 2.25), (name: "e", time: 3.0),
        ], spawnables: [
            (binding: "hero", prefab: "actors/hero.prefab.ron", start: 0.5, end: Some(1.5)),
            (binding: "prop", prefab: "actors/prop.prefab.ron", start: 2.0, keep_on_stop: true),
        ], restore_on_stop: true)"#,
    )
    .expect("valid v5")
}

#[derive(Resource, Default)]
struct Seen(Vec<(String, bool)>);

fn collect(mut reader: MessageReader<MarkerReached>, mut seen: ResMut<Seen>) {
    for m in reader.read() {
        seen.0.push((m.name.clone(), m.skipped));
    }
}

#[derive(Resource, Default)]
struct Ended(Vec<CutsceneEnded>);

fn collect_ended(mut reader: MessageReader<CutsceneEnded>, mut seen: ResMut<Ended>) {
    seen.0.extend(reader.read().copied());
}

fn input_config() -> InputConfig {
    InputConfig::new()
        .action("Jump", ActionKind::Button)
        .action(SKIP_ACTION, ActionKind::Button)
        .binding(InputBinding::new("Jump", InputSource::key("Space")))
        .context(CUTSCENE_CONTEXT, InputContext::new(50))
        .binding_in(
            CUTSCENE_CONTEXT,
            InputBinding::new(SKIP_ACTION, InputSource::key("Escape")),
        )
}

struct Rig {
    app: App,
    player: Entity,
    door: Entity,
}

fn rig(root: &Path) -> Rig {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        regia_scene::ScenePlugin,
        InputPlugin::new(input_config()),
        SequencerPlugin,
    ))
    .insert_resource(TimeUpdateStrategy::ManualDuration(STEP))
    .init_resource::<Seen>()
    .init_resource::<Ended>()
    .add_systems(Update, (collect, collect_ended));
    app.world_mut().resource_mut::<SequenceSpawnables>().project_root = Some(root.to_path_buf());
    let door = app
        .world_mut()
        .spawn((
            EntityGuid(Guid::from_u128(0xd00d)),
            Transform {
                translation: Vec3::splat(7.0),
                ..Transform::default()
            },
        ))
        .id();
    let mut player = SequencePlayer::new(cinematic());
    player.end = EndPolicy::Hold;
    let player = app
        .world_mut()
        .spawn((
            player,
            SequenceBindings::default().with("door", Binding::Entity(door)),
        ))
        .id();
    app.update();
    Rig { app, player, door }
}

fn time(app: &App, player: Entity) -> f64 {
    app.world().get::<SequencePlayer>(player).expect("player").time()
}

fn actor(app: &App, player: Entity, binding: &str) -> Option<Entity> {
    app.world()
        .resource::<SequenceSpawnables>()
        .actor(player, binding)
        .map(|a| a.entity)
        .filter(|&e| app.world().get_entity(e).is_ok())
}

fn position(app: &App, entity: Entity) -> Vec3 {
    app.world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation
}

fn key(app: &mut App, name: &str, state: ButtonState) {
    app.world_mut().write_message(KeyboardInput {
        window: Entity::PLACEHOLDER,
        physical_key: name.to_string(),
        logical_key: name.to_string(),
        state,
        repeat: false,
        timestamp: regia_platform::InputTimestamp::ZERO,
    });
}

fn gameplay_enabled(app: &App) -> bool {
    app.world().resource::<InputContexts>().is_enabled(0, "Gameplay")
}

fn play(rig: &mut Rig) {
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("player")
        .play();
}

#[test]
fn a_spawned_actor_exists_exactly_for_its_section_range() {
    let root = project();
    let mut rig = rig(&root);
    play(&mut rig);
    let mut seen_hero = 0;
    for _ in 0..40 {
        rig.app.update();
        let t = time(&rig.app, rig.player);
        let hero = actor(&rig.app, rig.player, "hero");
        let expected = (0.5..1.5).contains(&t);
        assert_eq!(hero.is_some(), expected, "hero at t = {t}");
        if let Some(hero) = hero {
            seen_hero += 1;
            // The track wrote it on the frame it appeared.
            let x = position(&rig.app, hero).x;
            let want = ((t - 0.5) / 1.0 * 5.0) as f32;
            assert!((x - want).abs() < 1.0e-4, "hero x {x} at t = {t}");
            // The prefab's entities hang under the wrapper.
            assert!(
                rig.app
                    .world()
                    .get::<Children>(hero)
                    .is_some_and(|c| !c.is_empty())
            );
        }
        let prop = actor(&rig.app, rig.player, "prop");
        assert_eq!(prop.is_some(), t >= 2.0, "prop at t = {t}");
    }
    assert_eq!(seen_hero, 10, "0.5, 0.6, ..., 1.4");
    let status = rig
        .app
        .world()
        .get::<SequencePlayer>(rig.player)
        .expect("p")
        .status();
    assert_eq!(status, PlaybackStatus::Finished, "held at the end");
    assert!(
        actor(&rig.app, rig.player, "prop").is_some(),
        "the end frame holds the open-ended actor"
    );
    // Stop: the hero is long gone, the prop is kept (keep_on_stop) and no longer the player's.
    let prop = actor(&rig.app, rig.player, "prop").expect("prop");
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .stop();
    rig.app.update();
    assert!(
        rig.app
            .world()
            .resource::<SequenceSpawnables>()
            .live(rig.player)
            .is_none()
    );
    assert!(rig.app.world().get_entity(prop).is_ok(), "kept on stop");
    assert!(
        rig.app
            .world()
            .get::<regia_framework::sequencer::SpawnedBySequence>(prop)
            .is_none()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_stop_in_range_despawns_and_a_seek_out_of_range_despawns_even_kept_actors() {
    let root = project();
    let mut rig = rig(&root);
    play(&mut rig);
    for _ in 0..8 {
        rig.app.update();
    }
    let hero = actor(&rig.app, rig.player, "hero").expect("hero at 0.8");
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .stop();
    rig.app.update();
    assert!(rig.app.world().get_entity(hero).is_err(), "despawned on stop");
    // A seek out of a kept actor's range despawns it (keep_on_stop only applies to a stop).
    play(&mut rig);
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .seek(2.5);
    rig.app.update();
    let prop = actor(&rig.app, rig.player, "prop").expect("prop at 2.5");
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .seek(1.0);
    rig.app.update();
    assert!(
        rig.app.world().get_entity(prop).is_err(),
        "out of range by a seek"
    );
    assert!(
        actor(&rig.app, rig.player, "hero").is_some(),
        "back in the hero's range"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Every frame's spawned guids, positions and markers, folded into one number.
fn run_hash(root: &Path) -> (u64, Vec<(String, bool)>) {
    let mut rig = rig(root);
    play(&mut rig);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut fold = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for _ in 0..35 {
        rig.app.update();
        fold(&time(&rig.app, rig.player).to_le_bytes());
        for binding in ["hero", "prop"] {
            if let Some(entity) = actor(&rig.app, rig.player, binding) {
                let guid = rig.app.world().get::<EntityGuid>(entity).expect("guid").0;
                fold(&guid.as_u128().to_le_bytes());
                for v in position(&rig.app, entity).to_array() {
                    fold(&v.to_le_bytes());
                }
            }
        }
        for v in position(&rig.app, rig.door).to_array() {
            fold(&v.to_le_bytes());
        }
    }
    let markers = rig.app.world().resource::<Seen>().0.clone();
    for (name, _) in &markers {
        fold(name.as_bytes());
    }
    (hash, markers)
}

#[test]
fn two_runs_give_the_same_state_hash() {
    let root = project();
    let (a, markers_a) = run_hash(&root);
    let (b, markers_b) = run_hash(&root);
    assert_eq!(a, b);
    assert_eq!(markers_a, markers_b);
    let names: Vec<&str> = markers_a.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "d", "e"]);
    let _ = std::fs::remove_dir_all(root);
}

/// The state a cutscene leaves behind once it handed back.
#[derive(Debug, PartialEq)]
struct Final {
    markers: Vec<String>,
    door: Vec3,
    kept_props: Vec<(u128, Vec3)>,
    heroes: usize,
    gameplay: bool,
}

fn final_state(rig: &mut Rig) -> Final {
    let world = rig.app.world_mut();
    let markers = world
        .resource::<Seen>()
        .0
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    let mut kept_props = Vec::new();
    let mut heroes = 0;
    let mut query = world.query::<(&regia_scene::components::Name, &EntityGuid, &Transform)>();
    for (name, guid, transform) in query.iter(world) {
        match name.as_str() {
            "prop" => kept_props.push((guid.0.as_u128(), transform.translation)),
            "hero" => heroes += 1,
            _ => {}
        }
    }
    kept_props.sort_by_key(|(g, _)| *g);
    let door = world.get::<Transform>(rig.door).expect("door").translation;
    Final {
        markers,
        door,
        kept_props,
        heroes,
        gameplay: world.resource::<InputContexts>().is_enabled(0, "Gameplay"),
    }
}

fn no_restore(rig: &mut Rig) {
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .restore_on_stop = false;
}

fn cutscene_to_end(root: &Path, skip_after: Option<usize>) -> (Final, usize, usize) {
    let mut rig = rig(root);
    no_restore(&mut rig);
    let settings = CutsceneSettings {
        letterbox_secs: 0.0,
        ..CutsceneSettings::default()
    };
    start_cutscene(rig.app.world_mut(), rig.player, settings).expect("start");
    let mut end_frame = usize::MAX;
    for frame in 0..60 {
        if Some(frame) == skip_after {
            skip_cutscene(rig.app.world_mut(), rig.player).expect("skip");
        }
        rig.app.update();
        if cutscene_state(rig.app.world(), rig.player).is_none() {
            end_frame = frame;
            break;
        }
    }
    assert!(
        cutscene_state(rig.app.world(), rig.player).is_none(),
        "handed back and ended"
    );
    // One more frame so the last markers reach the reader.
    rig.app.update();
    let skipped = rig
        .app
        .world()
        .resource::<Seen>()
        .0
        .iter()
        .filter(|(_, s)| *s)
        .count();
    (final_state(&mut rig), end_frame, skipped)
}

#[test]
fn a_skip_at_any_of_20_times_ends_in_the_state_of_playing_to_the_end() {
    let root = project();
    let (played, played_end, played_skipped) = cutscene_to_end(&root, None);
    assert_eq!(played.markers, ["a", "b", "c", "d", "e"]);
    assert_eq!(
        played.door,
        Vec3::new(0.0, 0.0, 9.0),
        "the last frame's value stays"
    );
    assert_eq!(played.kept_props.len(), 1, "the kept prop stays");
    assert_eq!(played.heroes, 0);
    assert!(played.gameplay, "input handed back");
    assert_eq!(played_skipped, 0);
    assert!(played_end >= 30, "played through: {played_end}");
    for skip in 0..20 {
        // Before update `skip` the clock is at `skip / 10` s (before update 0 it waits in the
        // pre-roll at 0, its zero markers not fired yet).
        let at = skip as f32 / 10.0;
        let (skipped, end, flagged) = cutscene_to_end(&root, Some(skip));
        assert_eq!(skipped, played, "skip after frame {skip}");
        assert!(
            end <= skip + 2,
            "the skip ended it at once: frame {end} after {skip}"
        );
        let left = [0.0f32, 0.75, 1.5, 2.25, 3.0]
            .iter()
            .filter(|&&m| m > at || (skip == 0 && m == 0.0))
            .count();
        assert_eq!(flagged, left, "markers after {at} s arrive skipped (skip {skip})");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skipped_markers_arrive_in_order_with_the_flag() {
    let root = project();
    let mut rig = rig(&root);
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("start");
    for _ in 0..9 {
        rig.app.update();
    }
    // t = 0.8: a and b played.
    assert!(skip_cutscene(rig.app.world_mut(), rig.player).expect("skip"));
    for _ in 0..10 {
        rig.app.update();
    }
    let seen = rig.app.world().resource::<Seen>().0.clone();
    assert_eq!(
        seen,
        [
            ("a".to_string(), false),
            ("b".to_string(), false),
            ("c".to_string(), true),
            ("d".to_string(), true),
            ("e".to_string(), true),
        ]
    );
    let ended = rig.app.world().resource::<Ended>().0.clone();
    assert_eq!(ended.len(), 1);
    assert!(ended[0].skipped && !ended[0].stopped);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn input_is_locked_during_play_and_restored_after_skip_and_stop() {
    let root = project();
    let mut rig = rig(&root);
    assert!(gameplay_enabled(&rig.app));
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("start");
    assert!(!gameplay_enabled(&rig.app), "locked from the start");
    key(&mut rig.app, "Space", ButtonState::Pressed);
    rig.app.update();
    assert!(
        !rig.app.world().resource::<Actions>().pressed("Jump"),
        "gameplay reads nothing"
    );
    key(&mut rig.app, "Space", ButtonState::Released);
    rig.app.update();
    // The skip key (bound in the cutscene context) skips.
    key(&mut rig.app, "Escape", ButtonState::Pressed);
    rig.app.update();
    key(&mut rig.app, "Escape", ButtonState::Released);
    for _ in 0..12 {
        rig.app.update();
    }
    let state = cutscene_state(rig.app.world(), rig.player);
    assert!(state.is_none(), "skipped and handed back: {state:?}");
    assert!(gameplay_enabled(&rig.app), "restored after the skip");
    key(&mut rig.app, "Space", ButtonState::Pressed);
    rig.app.update();
    assert!(
        rig.app.world().resource::<Actions>().pressed("Jump"),
        "gameplay reads again"
    );
    key(&mut rig.app, "Space", ButtonState::Released);
    rig.app.update();

    // Stop mid-play.
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("start again");
    for _ in 0..5 {
        rig.app.update();
    }
    assert!(!gameplay_enabled(&rig.app));
    stop_cutscene(rig.app.world_mut(), rig.player).expect("stop");
    assert!(gameplay_enabled(&rig.app), "restored by the stop");

    // Stop during the pre-roll (the player waits for its prefabs until the next frame).
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("start third");
    let state = cutscene_state(rig.app.world(), rig.player).expect("running");
    assert_eq!(state.phase, CutscenePhase::Preroll);
    assert!(
        rig.app
            .world()
            .get::<SequencePlayer>(rig.player)
            .expect("p")
            .is_prerolling()
    );
    assert!(!gameplay_enabled(&rig.app));
    stop_cutscene(rig.app.world_mut(), rig.player).expect("stop in pre-roll");
    assert!(
        gameplay_enabled(&rig.app),
        "restored by a stop during the pre-roll"
    );
    rig.app.update();
    assert!(
        rig.app
            .world()
            .resource::<SequenceSpawnables>()
            .live(rig.player)
            .is_none()
    );
    let ended = rig.app.world().resource::<Ended>().0.clone();
    assert_eq!(ended.iter().filter(|e| e.stopped).count(), 2);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_held_skip_with_a_fade_waits_for_the_hold_and_fades_through_black() {
    let root = project();
    let mut rig = rig(&root);
    let settings = CutsceneSettings {
        skip: SkipSettings {
            hold_secs: 0.5,
            fade_secs: 0.3,
            ..SkipSettings::default()
        },
        ..CutsceneSettings::default()
    };
    start_cutscene(rig.app.world_mut(), rig.player, settings).expect("start");
    rig.app.update();
    key(&mut rig.app, "Escape", ButtonState::Pressed);
    let mut faded_black = false;
    let mut skipped_at = None;
    for frame in 0..30 {
        rig.app.update();
        let Some(state) = cutscene_state(rig.app.world(), rig.player) else {
            break;
        };
        if state.fade >= 1.0 {
            faded_black = true;
        }
        if state.skipped && skipped_at.is_none() {
            skipped_at = Some(frame);
        }
        if frame < 4 {
            assert!(
                !state.skipped && state.phase != CutscenePhase::SkipFadeOut,
                "still holding at {frame}"
            );
        }
    }
    assert!(faded_black, "the fade reached black");
    assert!(
        skipped_at.is_some_and(|f| f >= 6),
        "hold 0.5 s then fade 0.3 s: {skipped_at:?}"
    );
    assert!(
        cutscene_state(rig.app.world(), rig.player).is_none(),
        "faded back in and ended"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn restore_on_stop_returns_the_prior_values() {
    let root = project();
    let mut rig = rig(&root);
    assert_eq!(position(&rig.app, rig.door), Vec3::splat(7.0));
    play(&mut rig);
    for _ in 0..10 {
        rig.app.update();
    }
    assert_ne!(position(&rig.app, rig.door), Vec3::splat(7.0), "animated");
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .stop();
    rig.app.update();
    assert_eq!(position(&rig.app, rig.door), Vec3::splat(7.0), "put back on stop");
    // Without restore_on_stop the last value stays.
    no_restore(&mut rig);
    play(&mut rig);
    for _ in 0..10 {
        rig.app.update();
    }
    let moved = position(&rig.app, rig.door);
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .stop();
    rig.app.update();
    assert_eq!(position(&rig.app, rig.door), moved);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_prefab_that_fails_to_load_skips_its_tracks_with_a_code() {
    let root = project();
    let mut rig = rig(&root);
    let mut sequence = cinematic();
    sequence.spawnables[0].prefab = "actors/missing.prefab.ron".into();
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .replace_sequence(sequence);
    play(&mut rig);
    for _ in 0..12 {
        rig.app.update();
    }
    let player = rig.app.world().get::<SequencePlayer>(rig.player).expect("p");
    let issues: Vec<String> = player.issues().map(|i| format!("{}:{i}", i.kind())).collect();
    assert!(
        issues
            .iter()
            .any(|i| i.starts_with("seq_spawn_failed:") && i.contains("prefab_missing")),
        "{issues:?}"
    );
    assert!(
        !issues.iter().any(|i| i.starts_with("unbound_target")),
        "{issues:?}"
    );
    assert!(actor(&rig.app, rig.player, "hero").is_none());
    assert!(player.time() > 1.0, "the rest of the sequence plays");
    assert!(
        position(&rig.app, rig.door).z > 0.0,
        "the hand-made binding still animates"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// A live edit that swaps in a sequence with spawnables while the player runs loads their prefabs
/// (the pre-roll ran at `play` for the old sequence only): the new actor appears in its range, and
/// a binding whose prefab fails is reported once instead of silently never spawning.
#[test]
fn a_live_edit_that_adds_spawnables_loads_and_spawns_them() {
    let root = project();
    let mut rig = rig(&root);
    let mut plain = cinematic();
    plain.spawnables.clear();
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .replace_sequence(plain);
    play(&mut rig);
    for _ in 0..3 {
        rig.app.update();
    }
    assert!(time(&rig.app, rig.player) < 0.5, "before the hero's range");
    let mut edited = cinematic();
    edited.spawnables[1].prefab = "actors/missing.prefab.ron".into();
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .replace_sequence(edited);
    for _ in 0..6 {
        rig.app.update();
    }
    let t = time(&rig.app, rig.player);
    assert!((0.5..1.5).contains(&t), "{t}");
    assert!(
        actor(&rig.app, rig.player, "hero").is_some(),
        "the added spawnable is spawned in its range"
    );
    let player = rig.app.world().get::<SequencePlayer>(rig.player).expect("p");
    let issues: Vec<String> = player.issues().map(|i| format!("{}:{i}", i.kind())).collect();
    assert_eq!(
        issues
            .iter()
            .filter(|i| i.starts_with("seq_spawn_failed:") && i.contains("prefab_missing"))
            .count(),
        1,
        "{issues:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn letterbox_bars_slide_in_and_out_and_the_hud_comes_back() {
    let root = project();
    let mut rig = rig(&root);
    let hud = rig
        .app
        .world_mut()
        .spawn((
            regia_framework::sequencer::HudLayer("hud".into()),
            regia_ui::node::NodeStyle::default(),
        ))
        .id();
    let settings = CutsceneSettings {
        letterbox: 0.1,
        letterbox_secs: 0.2,
        hide_hud: vec!["hud".into()],
        ..CutsceneSettings::default()
    };
    start_cutscene(rig.app.world_mut(), rig.player, settings).expect("start");
    let display = |app: &App| {
        app.world()
            .get::<regia_ui::node::NodeStyle>(hud)
            .expect("style")
            .0
            .display
    };
    assert_eq!(
        display(&rig.app),
        regia_ui::style::Display::None,
        "hidden at once"
    );
    rig.app.update();
    rig.app.update();
    rig.app.update();
    let state = cutscene_state(rig.app.world(), rig.player).expect("running");
    assert_eq!(state.letterbox, 1.0, "in after 0.2 s");
    assert_eq!(state.hud_hidden, 1);
    {
        let world = rig.app.world_mut();
        let mut roots = world.query::<&regia_ui::node::UiRoot>();
        assert_eq!(
            roots.iter(world).count(),
            1,
            "the letterbox root exists mid-cutscene"
        );
    }
    for _ in 0..40 {
        rig.app.update();
    }
    assert!(cutscene_state(rig.app.world(), rig.player).is_none());
    assert_eq!(display(&rig.app), regia_ui::style::Display::Flex, "shown again");
    let mut roots = rig.app.world_mut().query::<&regia_ui::node::UiRoot>();
    assert_eq!(roots.iter(rig.app.world()).count(), 0, "the overlay is gone");
    let _ = std::fs::remove_dir_all(root);
}

/// Two peers of one session: both start at the scheduled tick, a late joiner lands where the
/// tick says.
#[test]
fn net_cutscenes_start_at_the_tick_and_a_late_joiner_lands_at_the_right_time() {
    let root = project();
    let mut peers: Vec<Rig> = (0..3).map(|_| rig(&root)).collect();
    for peer in &mut peers {
        no_restore(peer);
        let player = peer.player;
        let mut net = peer.app.world_mut().resource_mut::<NetCutscenes>();
        net.tick_hz = 10.0;
        net.register("intro", player, CutsceneSettings::default());
    }
    // Peers 0 and 1 know the start before tick 5; peer 2 joins at tick 17.
    for tick in 0..30u64 {
        for (i, peer) in peers.iter_mut().enumerate() {
            if (i < 2 && tick == 2) || (i == 2 && tick == 17) {
                peer.app
                    .world_mut()
                    .resource_mut::<NetCutscenes>()
                    .schedule("intro", 5);
            }
            peer.app.world_mut().resource_mut::<NetCutscenes>().set_tick(tick);
            peer.app.update();
        }
        let t0 = time(&peers[0].app, peers[0].player);
        let t1 = time(&peers[1].app, peers[1].player);
        assert_eq!(t0, t1, "the two on-time peers agree at tick {tick}");
        if (5..30).contains(&tick) && t0 < 3.0 {
            assert!((t0 - (tick - 4) as f64 * 0.1).abs() < 1.0e-9, "tick {tick}: {t0}");
        }
        if tick >= 17 {
            let late = time(&peers[2].app, peers[2].player);
            assert_eq!(late, t0, "the late joiner lands with the others at tick {tick}");
        }
    }
    // The late joiner got the markers before its join as skipped.
    let late = peers[2].app.world().resource::<Seen>().0.clone();
    assert!(late.iter().any(|(n, s)| n == "b" && *s), "{late:?}");
    let on_time = peers[0].app.world().resource::<Seen>().0.clone();
    assert!(on_time.iter().all(|(_, s)| !s), "{on_time:?}");
    let names = |v: &[(String, bool)]| v.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
    assert_eq!(names(&late), names(&on_time));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_v5_file_round_trips_and_a_v4_file_loads_unchanged() {
    let sequence = cinematic();
    let text = sequence.to_ron().expect("save");
    assert_eq!(Sequence::from_ron(&text).expect("load"), sequence);
    assert!(text.contains("spawnables"));
    assert!(text.contains("keep_on_stop: true"));
    let v4 = r#"(version: 4, name: "old", tracks: [(name: "m", target: "x", property: "visible",
        keys: Bool([(time: 0.0, value: true)]))])"#;
    let old = Sequence::from_ron(v4).expect("v4 loads");
    assert!(old.spawnables.is_empty() && !old.restore_on_stop);
    let resaved = old.to_ron().expect("save");
    assert!(!resaved.contains("spawnables") && !resaved.contains("restore_on_stop"));
    let mut bad = cinematic();
    bad.spawnables
        .push(Spawnable::new("hero", "actors/x.prefab.ron", 0.0));
    assert!(bad.validate().is_err(), "duplicate binding");
    let mut escape = cinematic();
    escape.spawnables[0].prefab = "../outside.prefab.ron".into();
    assert!(escape.validate().is_err());
    let guid = Spawnable::new("x", "guid:000000000000000000000000000000ff", 0.0);
    assert_eq!(guid.prefab_guid(), Some(Guid::from_u128(0xff)));
    assert!(matches!(cinematic().tracks[0].keys, Channel::Vec3(_)));
}

#[test]
fn a_skip_clears_the_subtitles_unless_told_not_to() {
    let root = project();
    let mut rig = rig(&root);
    rig.app.init_resource::<regia_ui::SubtitleQueue>();
    let line = || regia_ui::SubtitleLine::new("guard", "Halt!", 0.0, 5.0);
    rig.app
        .world_mut()
        .resource_mut::<regia_ui::SubtitleQueue>()
        .push(line())
        .expect("queued");
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("start");
    rig.app.update();
    assert_eq!(rig.app.world().resource::<regia_ui::SubtitleQueue>().len(), 1);
    skip_cutscene(rig.app.world_mut(), rig.player).expect("skip");
    assert!(
        rig.app.world().resource::<regia_ui::SubtitleQueue>().is_empty(),
        "the skip stopped the subtitles"
    );
    for _ in 0..10 {
        rig.app.update();
    }
    // With stop_dialogue_on_skip off a skip leaves them alone.
    rig.app
        .world_mut()
        .resource_mut::<regia_ui::SubtitleQueue>()
        .push(line())
        .expect("queued");
    let keep = CutsceneSettings {
        stop_dialogue_on_skip: false,
        ..CutsceneSettings::default()
    };
    start_cutscene(rig.app.world_mut(), rig.player, keep).expect("start again");
    rig.app.update();
    skip_cutscene(rig.app.world_mut(), rig.player).expect("skip");
    assert_eq!(rig.app.world().resource::<regia_ui::SubtitleQueue>().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn overlapping_cutscenes_give_back_exactly_what_was_there_before_either() {
    let root = project();
    let mut rig = rig(&root);
    let hud = rig
        .app
        .world_mut()
        .spawn((
            regia_framework::sequencer::HudLayer("hud".into()),
            regia_ui::node::NodeStyle::default(),
        ))
        .id();
    let second = rig.app.world_mut().spawn(SequencePlayer::new(cinematic())).id();
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("first");
    rig.app.update();
    start_cutscene(rig.app.world_mut(), second, CutsceneSettings::default()).expect("second");
    rig.app.update();
    stop_cutscene(rig.app.world_mut(), rig.player).expect("stop the first");
    assert!(!gameplay_enabled(&rig.app), "the second still holds the input");
    let display = |app: &App| {
        app.world()
            .get::<regia_ui::node::NodeStyle>(hud)
            .expect("style")
            .0
            .display
    };
    assert_eq!(display(&rig.app), regia_ui::style::Display::None, "and the HUD");
    stop_cutscene(rig.app.world_mut(), second).expect("stop the second");
    assert!(gameplay_enabled(&rig.app), "the last one out gives Gameplay back");
    assert_eq!(display(&rig.app), regia_ui::style::Display::Flex);
    let _ = std::fs::remove_dir_all(root);
}

/// Two overlapping cutscenes with their own input contexts: when the later one ends first, the one
/// still running gets its own context back (its skip action is bound there), not the ended one's;
/// the last one out restores what was there before either.
#[test]
fn a_partial_unlock_gives_the_running_cutscene_its_own_context_back() {
    let root = project();
    let mut rig = rig(&root);
    let second = rig.app.world_mut().spawn(SequencePlayer::new(cinematic())).id();
    let with = |context: &str| CutsceneSettings {
        input_context: context.into(),
        ..CutsceneSettings::default()
    };
    let enabled = |app: &App| app.world().resource::<InputContexts>().enabled(0).to_vec();
    let before = enabled(&rig.app);
    start_cutscene(rig.app.world_mut(), rig.player, with("cutscene_a")).expect("first");
    rig.app.update();
    start_cutscene(rig.app.world_mut(), second, with("cutscene_b")).expect("second");
    rig.app.update();
    assert_eq!(enabled(&rig.app), vec!["cutscene_b".to_string()]);
    stop_cutscene(rig.app.world_mut(), second).expect("stop the second");
    assert_eq!(
        enabled(&rig.app),
        vec!["cutscene_a".to_string()],
        "the first is still running"
    );
    stop_cutscene(rig.app.world_mut(), rig.player).expect("stop the first");
    assert_eq!(enabled(&rig.app), before, "the last one out restores");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn restore_on_stop_leaves_a_kept_actor_where_the_sequence_put_it() {
    let root = project();
    let mut rig = rig(&root);
    play(&mut rig);
    for _ in 0..32 {
        rig.app.update();
    }
    let prop = actor(&rig.app, rig.player, "prop").expect("the prop at the held end");
    assert_eq!(position(&rig.app, prop), Vec3::new(1.0, 2.0, 0.0));
    rig.app
        .world_mut()
        .get_mut::<SequencePlayer>(rig.player)
        .expect("p")
        .stop();
    rig.app.update();
    rig.app.update();
    assert!(rig.app.world().get_entity(prop).is_ok(), "kept");
    assert_eq!(
        position(&rig.app, prop),
        Vec3::new(1.0, 2.0, 0.0),
        "not snapped back to where it spawned"
    );
    assert_eq!(
        position(&rig.app, rig.door),
        Vec3::splat(7.0),
        "the door is put back"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_skip_button_held_from_before_the_cutscene_does_not_skip_it() {
    let root = project();
    let mut rig = rig(&root);
    // Escape is bound only in the cutscene context, so hold it under that context first.
    rig.app
        .world_mut()
        .resource_mut::<InputContexts>()
        .push(0, CUTSCENE_CONTEXT);
    key(&mut rig.app, "Escape", ButtonState::Pressed);
    rig.app.update();
    assert!(rig.app.world().resource::<Actions>().pressed(SKIP_ACTION));
    start_cutscene(rig.app.world_mut(), rig.player, CutsceneSettings::default()).expect("start");
    for _ in 0..5 {
        rig.app.update();
    }
    let state = cutscene_state(rig.app.world(), rig.player).expect("running");
    assert!(!state.skipped, "the held button did not skip");
    key(&mut rig.app, "Escape", ButtonState::Released);
    rig.app.update();
    key(&mut rig.app, "Escape", ButtonState::Pressed);
    rig.app.update();
    let state = cutscene_state(rig.app.world(), rig.player).expect("running");
    assert!(state.skipped, "a fresh press skips");
    let _ = std::fs::remove_dir_all(root);
}
