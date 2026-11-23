// SPDX-License-Identifier: MIT-0
//! The course plays every locomotion clip, moves the hero, and repeats bit for bit.

use mm_locomotion::{Course, CourseLog, headless_app, hero};
use regia_animation::components::Animator;
use regia_animation::motion_matching::{MotionMatchPlayer, MotionMatching};
use regia_scene::Transform;

fn run(frames: u32) -> (CourseLog, Vec<u32>) {
    let mut app = headless_app();
    for _ in 0..frames {
        app.update();
    }
    let world = app.world_mut();
    let hero = hero(world).expect("hero");
    let pose: Vec<u32> = world
        .get::<Animator>(hero)
        .expect("animator")
        .evaluated_pose()
        .translations
        .iter()
        .flat_map(|v| v.to_array())
        .map(f32::to_bits)
        .collect();
    (world.resource::<CourseLog>().clone(), pose)
}

#[test]
fn one_lap_plays_each_clip_and_moves_the_hero() {
    let (log, pose) = run(345);
    for clip in ["walk", "turn_right", "run", "turn_left", "stop"] {
        assert!(
            log.clips.iter().any(|c| c == clip),
            "{clip} played: {:?}",
            log.clips
        );
    }
    assert!(log.distance > 10.0, "the hero covered {} m", log.distance);
    assert_eq!(pose.len(), 9, "three joints");
}

#[test]
fn the_course_is_deterministic() {
    assert_eq!(run(400), run(400));
}

#[test]
fn the_hero_carries_the_whole_motion_matching_stack() {
    let mut app = headless_app();
    app.update();
    app.update();
    let world = app.world_mut();
    let hero = hero(world).expect("hero");
    assert!(world.get::<MotionMatching>(hero).is_some());
    assert!(world.get::<MotionMatchPlayer>(hero).is_some());
}

#[test]
fn the_predictor_writes_the_path_from_the_course_velocity() {
    use regia_animation::motion_matching::TrajectoryPredictor;
    let mut app = headless_app();
    // Into the walk leg (stand is the first second).
    for _ in 0..50 {
        app.update();
    }
    let world = app.world_mut();
    let hero = hero(world).expect("hero");
    let predictor = world.get::<TrajectoryPredictor>(hero).expect("predictor").clone();
    let matching = world.get::<MotionMatching>(hero).expect("matching");
    let times = matching.database.schema().trajectory_times.clone();
    assert_eq!(
        matching.trajectory,
        predictor.trajectory(&times),
        "the matcher searches with the predicted path"
    );
    assert!((predictor.desired_velocity.length() - 1.5).abs() < 1e-5);
    let far = matching.trajectory.last().expect("point");
    assert!(
        far.position.y > 1.0,
        "the path leads ahead while walking: {far:?}"
    );
}

#[test]
fn course_log_separates_host_updates_from_simulation_steps() {
    let mut app = headless_app();
    app.update();
    app.update();
    let before = app.world().resource::<CourseLog>().clone();
    let elapsed_before = app.world().resource::<Course>().elapsed;
    let entity = hero(app.world_mut()).expect("hero");
    let before_transform = *app.world().get::<Transform>(entity).expect("transform");
    let before_matcher = app
        .world()
        .get::<MotionMatching>(entity)
        .expect("matcher")
        .matcher
        .clone();
    let before_player_pose = app
        .world()
        .get::<MotionMatchPlayer>(entity)
        .expect("player")
        .pose()
        .clone();
    let before_evaluated_pose = app
        .world()
        .get::<Animator>(entity)
        .expect("animator")
        .evaluated_pose()
        .clone();
    app.world_mut().resource_mut::<regia_app::Time>().paused = true;
    for _ in 0..4 {
        app.update();
    }
    let after = app.world().resource::<CourseLog>();
    assert_eq!(after.frames - before.frames, 4, "host updates include pause");
    assert_eq!(
        after.simulation_frames, before.simulation_frames,
        "paused host updates are not simulation steps"
    );
    assert_eq!(
        app.world().resource::<Course>().elapsed,
        elapsed_before,
        "the sample course uses simulation time"
    );
    assert_eq!(
        app.world().get::<Transform>(entity).expect("transform"),
        &before_transform
    );
    assert_eq!(
        &app.world()
            .get::<MotionMatching>(entity)
            .expect("matcher")
            .matcher,
        &before_matcher,
        "zero-delta host updates do not change matcher/player state"
    );
    assert_eq!(
        app.world()
            .get::<MotionMatchPlayer>(entity)
            .expect("player")
            .pose(),
        &before_player_pose
    );
    assert_eq!(
        app.world()
            .get::<Animator>(entity)
            .expect("animator")
            .evaluated_pose(),
        &before_evaluated_pose
    );
}
