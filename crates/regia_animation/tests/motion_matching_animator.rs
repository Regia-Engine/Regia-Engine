// SPDX-License-Identifier: MPL-2.0
//! Motion matching through the `Animator` (ADR-2409): a rig whose `MotionDatabaseRef` resolves
//! through `MotionDatabases`, played by `MotionMatchPlugin` and shown by its animator's
//! motion-matching stage, picks the same frames and shows bit-identical poses to a matcher and
//! player stepped by hand at the same fixed steps. The stage's weight parameter and fade hand the
//! body back to the graph.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::motion_matching::player::step_player;
use regia_animation::motion_matching::procedural::{arc_trajectory, procedural_locomotion};
use regia_animation::motion_matching::{
    AnimatorMotionMatch, DEFAULT_TRAJECTORY_TIMES, LoadedMotionDatabase, MotionDatabaseRef,
    MotionDatabaseRefState, MotionDatabases, MotionMatchPlayer, MotionMatchPlugin, MotionMatching,
    TrajectoryPoint,
};
use regia_animation::prelude::*;
use regia_animation::{Pose, Skeleton};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

const PATH: &str = "procedural/locomotion.motiondb.ron";

/// The desired path at frame `step`: stand, walk, turn right, run, turn left, walk (60 frames each).
fn path_at(step: u32) -> Vec<TrajectoryPoint> {
    let (speed, turn) = [
        (0.0, 0.0),
        (1.5, 0.0),
        (1.5, 1.2),
        (4.0, 0.0),
        (1.5, -1.2),
        (1.5, 0.0),
    ][((step / 60) % 6) as usize];
    arc_trajectory(&DEFAULT_TRAJECTORY_TIMES, speed, turn)
}

/// An app at a fixed 1/30 s step with the procedural database registered at [`PATH`].
fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin, MotionMatchPlugin));
    regia_app::enable_fixed_step(&mut app, 30.0);
    let set = procedural_locomotion();
    let loaded = LoadedMotionDatabase::built(
        set.build().expect("database"),
        set.skeleton.clone(),
        set.entry_clips(),
    );
    app.world_mut()
        .resource_mut::<MotionDatabases>()
        .insert(PATH, loaded);
    // The first frame has no delta.
    app.update();
    app
}

/// A rig on the procedural skeleton playing an idle clip through its graph, with motion matching.
fn spawn_rig(app: &mut App, stage: AnimatorMotionMatch) -> Entity {
    let skeleton: Arc<Skeleton> = Arc::new(procedural_locomotion().skeleton);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let idle = AnimationClip::empty("graph_idle", 1.0);
    app.world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(idle, true),
            MotionDatabaseRef::path(PATH),
            stage,
        ))
        .id()
}

fn set_path(app: &mut App, rig: Entity, path: &[TrajectoryPoint]) {
    if let Some(mut matching) = app.world_mut().get_mut::<MotionMatching>(rig) {
        matching.trajectory = path.to_vec();
    }
}

fn bits(pose: &Pose) -> Vec<u32> {
    pose.translations
        .iter()
        .flat_map(|v| v.to_array())
        .chain(pose.rotations.iter().flat_map(|q| q.to_array()))
        .chain(pose.scales.iter().flat_map(|v| v.to_array()))
        .map(f32::to_bits)
        .collect()
}

#[test]
fn the_reference_resolves_into_a_matcher_and_a_player() {
    let mut app = app();
    let rig = spawn_rig(&mut app, AnimatorMotionMatch::default());
    app.update();
    let world = app.world();
    assert!(
        world.get::<MotionMatching>(rig).is_some(),
        "MotionMatching inserted"
    );
    assert!(
        world.get::<MotionMatchPlayer>(rig).is_some(),
        "player inserted (the database has clips)"
    );
    let state = world.get::<MotionDatabaseRefState>(rig).expect("state");
    assert_eq!(state.path.as_deref(), Some(PATH));
    assert_eq!(state.error, None);
    assert_eq!(state.generation, 1);

    // An unknown path records its code and inserts nothing.
    let missing = app
        .world_mut()
        .spawn(MotionDatabaseRef::path("nope.motiondb.ron"))
        .id();
    app.update();
    let state = app.world().get::<MotionDatabaseRefState>(missing).expect("state");
    assert_eq!(state.error, Some("unknown_database"));
    assert!(app.world().get::<MotionMatching>(missing).is_none());

    // A reload (a new generation at the path) swaps the database and resets the matcher.
    let set = procedural_locomotion();
    let again = LoadedMotionDatabase::built(
        set.build().expect("database"),
        set.skeleton.clone(),
        set.entry_clips(),
    );
    app.world_mut()
        .resource_mut::<MotionDatabases>()
        .insert(PATH, again);
    app.update();
    let state = app.world().get::<MotionDatabaseRefState>(rig).expect("state");
    assert_eq!(state.generation, 2);
    let shared = app
        .world()
        .resource::<MotionDatabases>()
        .get(PATH)
        .expect("db")
        .database
        .clone();
    let matching = app.world().get::<MotionMatching>(rig).expect("matching");
    assert!(
        Arc::ptr_eq(&matching.database, &shared),
        "the reloaded database is in use"
    );
}

#[test]
fn animator_driven_matching_shows_the_direct_players_poses() {
    let mut app = app();
    let rig = spawn_rig(&mut app, AnimatorMotionMatch::default());
    set_path(&mut app, rig, &path_at(0));
    app.update();
    // The hand-stepped twin starts from the app's state after the first frame.
    let world = app.world();
    let mut matching = world.get::<MotionMatching>(rig).expect("matching").clone();
    let mut player = world.get::<MotionMatchPlayer>(rig).expect("player").clone();
    let mut jumps = 0u64;
    for step in 1..360u32 {
        let path = path_at(step);
        set_path(&mut app, rig, &path);
        app.update();
        // The same frame time the plugin's systems read.
        let dt = app.world().resource::<regia_app::Time>().delta_secs();
        matching.trajectory = path;
        matching.step(dt).expect("direct step");
        step_player(&mut matching, &mut player, dt);

        let world = app.world();
        let driven = world.get::<MotionMatching>(rig).expect("matching");
        let a = driven.last_step.expect("stepped");
        let b = matching.last_step.expect("stepped");
        assert_eq!(a.playback.frame, b.playback.frame, "step {step}: frame");
        assert_eq!(
            a.playback.time.to_bits(),
            b.playback.time.to_bits(),
            "step {step}: time"
        );
        assert_eq!(a.jump.is_some(), b.jump.is_some(), "step {step}: jump");
        jumps += u64::from(a.jump.is_some());
        let animator = world.get::<Animator>(rig).expect("animator");
        assert_eq!(
            bits(animator.evaluated_pose()),
            bits(player.pose()),
            "step {step}: the animator shows the player's pose"
        );
        let shown = world.get::<MotionMatchPlayer>(rig).expect("player");
        assert_eq!(
            bits(shown.pose()),
            bits(player.pose()),
            "step {step}: player state"
        );
    }
    assert!(jumps >= 4, "the path changes make it jump ({jumps})");
    let output = app
        .world()
        .get::<RootMotionOutput>(rig)
        .expect("root motion output");
    assert!(output.delta.is_finite(), "root motion reached the rig's output");
}

/// ADR-4477 / audit animation1-determinism M8: all fixed-substep root deltas reach the
/// presentation consumer exactly once; two-step frames do not discard the first delta.
#[test]
fn root_motion_is_aggregated_across_fixed_substeps() {
    fn run(frames: usize, presentation: Duration) -> ((Vec3, regia_core::math::Quat, f32), Vec<u32>) {
        let mut app = app();
        app.insert_resource(regia_app::FixedTime::default());
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(presentation));
        let rig = spawn_rig(&mut app, AnimatorMotionMatch::default());
        set_path(&mut app, rig, &path_at(70));
        let mut total = RootMotionOutput::default();
        for _ in 0..frames {
            app.update();
            if let Some(mut output) = app.world_mut().get_mut::<RootMotionOutput>(rig) {
                let (translation, rotation, seconds) = output.take();
                total.accumulate(translation, rotation, seconds);
            }
        }
        let player = app.world().get::<MotionMatchPlayer>(rig).expect("player");
        (total.take(), bits(player.pose()))
    }

    let fast = run(60, Duration::from_secs_f64(1.0 / 60.0));
    let slow = run(30, Duration::from_secs_f64(1.0 / 30.0));
    assert_eq!(
        fast.0.0.to_array().map(f32::to_bits),
        slow.0.0.to_array().map(f32::to_bits)
    );
    assert_eq!(
        fast.0.1.to_array().map(f32::to_bits),
        slow.0.1.to_array().map(f32::to_bits)
    );
    assert_eq!(fast.1, slow.1);
}

#[test]
fn the_weight_parameter_hands_the_body_back_to_the_graph_with_a_fade() {
    let mut app = app();
    let stage = AnimatorMotionMatch {
        weight_param: "MotionMatch".into(),
        fade: 0.2,
        ..AnimatorMotionMatch::default()
    };
    let rig = spawn_rig(&mut app, stage);
    // A twin with no stage shows what the graph alone evaluates.
    let skeleton: Arc<Skeleton> = Arc::new(procedural_locomotion().skeleton);
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let plain = app
        .world_mut()
        .spawn((
            Transform::default(),
            binding,
            Animator::single(AnimationClip::empty("graph_idle", 1.0), true),
        ))
        .id();
    for _ in 0..30 {
        set_path(&mut app, rig, &path_at(70));
        app.update();
    }
    let weight = |app: &App| {
        app.world()
            .get::<AnimatorMotionMatch>(rig)
            .expect("stage")
            .current
    };
    let pose =
        |app: &App, e: Entity| bits(app.world().get::<Animator>(e).expect("animator").evaluated_pose());
    assert_eq!(weight(&app), 0.0, "an absent parameter is off");
    assert_eq!(
        pose(&app, rig),
        pose(&app, plain),
        "the graph's pose is untouched at weight 0"
    );

    app.world_mut()
        .get_mut::<Animator>(rig)
        .expect("animator")
        .set("MotionMatch", true);
    let mut weights = Vec::new();
    for _ in 0..8 {
        set_path(&mut app, rig, &path_at(70));
        app.update();
        weights.push(weight(&app));
    }
    let expected = [
        1.0 / 6.0,
        2.0 / 6.0,
        3.0 / 6.0,
        4.0 / 6.0,
        5.0 / 6.0,
        1.0,
        1.0,
        1.0,
    ];
    for (got, want) in weights.iter().zip(expected) {
        assert!(
            (got - want).abs() < 1e-5,
            "fade in over 0.2 s at 1/30 s: {weights:?}"
        );
    }
    let player = bits(app.world().get::<MotionMatchPlayer>(rig).expect("player").pose());
    assert_eq!(pose(&app, rig), player, "full weight shows the player");
}
