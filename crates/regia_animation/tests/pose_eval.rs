// SPDX-License-Identifier: MPL-2.0
//! Side-effect-free pose evaluation (ADR-2551) against a recorded run of the live animator: for
//! every frame, the snapshot taken before the frame, evaluated purely with that frame's
//! parameters and delta, equals the animator's evaluated pose bit for bit. The run walks, runs,
//! jumps twice (cross-fades in and out of a one-shot) and eases an upper-body layer in and out.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::pose_eval::demo::{AIM, JUMP, SPEED, demo_net_character};
use regia_animation::pose_eval::{
    AnimStateSnapshot, CaptureError, ParamLanes, PoseRequest, capture_state, evaluate_pose, next_eval_delta,
    restore_state, restore_state_ahead,
};
use regia_animation::prelude::*;
use regia_app::prelude::*;
use regia_scene::Transform;

const STEP: Duration = Duration::from_nanos(16_666_667);

fn app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(STEP));
    app.update();
    let (skeleton, graph, clips) = demo_net_character().expect("demo rig");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, Animator::graph(graph, clips)))
        .id();
    (app, rig)
}

/// What the test drives on frame `f`.
fn drive(animator: &mut Animator, f: u32) {
    let speed = match f {
        0..40 => 0.0,
        40..120 => (f - 40) as f32 * 0.04,
        _ => 3.2 - (f as f32 - 120.0) * 0.01,
    };
    animator.set(SPEED, speed);
    animator.set(AIM, if (90..200).contains(&f) { 1.0 } else { 0.0 });
    if f == 60 || f == 150 {
        animator.trigger(JUMP);
    }
}

fn same_bits(a: &Pose, b: &Pose) -> bool {
    let v = |x: &[regia_core::math::Vec3], y: &[regia_core::math::Vec3]| {
        x.len() == y.len()
            && x.iter()
                .zip(y)
                .all(|(p, q)| p.to_array().map(f32::to_bits) == q.to_array().map(f32::to_bits))
    };
    v(&a.translations, &b.translations)
        && v(&a.scales, &b.scales)
        && a.rotations.len() == b.rotations.len()
        && a.rotations
            .iter()
            .zip(&b.rotations)
            .all(|(p, q)| p.to_array().map(f32::to_bits) == q.to_array().map(f32::to_bits))
}

#[test]
fn pure_evaluation_matches_the_live_animator_bit_for_bit() {
    let (mut app, rig) = app();
    let mut checked = 0;
    let mut states_seen = std::collections::BTreeSet::new();
    let mut crossfades = 0;
    for f in 0..260u32 {
        let before = {
            let mut animator = app.world_mut().get_mut::<Animator>(rig).expect("animator");
            drive(&mut animator, f);
            let snapshot = capture_state(&animator);
            let params = ParamLanes::capture(&animator.parameters, None);
            (snapshot, params, animator.clone())
        };
        app.update();
        let world = app.world();
        // The frame's delta as the evaluate system read it.
        let delta_secs = world.resource::<regia_app::Time>().delta_secs();
        let animator = world.get::<Animator>(rig).expect("animator");
        let binding = world.get::<SkinnedMesh>(rig).expect("rig");
        let (snapshot, params, before_animator) = before;
        let delta = next_eval_delta(&before_animator, delta_secs);
        let snapshot = match snapshot {
            Ok(s) => s,
            Err(CaptureError::NotEvaluated) if f == 0 => continue,
            Err(e) => panic!("frame {f}: capture failed: {e}"),
        };
        // Words: [version, kind, machine current state, machine flags, ...] (the layer's base is
        // the machine): note the states and the cross-fades the run went through.
        states_seen.insert(snapshot.words()[2]);
        if snapshot.words()[3] & 2 != 0 {
            crossfades += 1;
        }
        let params = params.to_set();
        let request = PoseRequest {
            skeleton: &binding.skeleton,
            source: &animator.source,
            state: &snapshot,
            params: &params,
            delta,
            rig,
            socket_targets: None,
        };
        let mut pure = Pose::default();
        evaluate_pose(&request, &mut pure).expect("pure evaluation");
        assert!(
            same_bits(&pure, animator.evaluated_pose()),
            "frame {f}: the pure pose differs from the live animator's"
        );
        // From the words alone (what a link carries) the pose is the same: the words are the
        // whole state, layer easing included.
        let from_words = AnimStateSnapshot::from_words(snapshot.words());
        assert_eq!(from_words, snapshot);
        let mut wire = Pose::default();
        evaluate_pose(
            &PoseRequest {
                state: &from_words,
                ..request
            },
            &mut wire,
        )
        .expect("words");
        assert!(same_bits(&wire, &pure), "frame {f}: words alone differ");
        checked += 1;
    }
    assert_eq!(checked, 259);
    assert_eq!(
        states_seen.into_iter().collect::<Vec<_>>(),
        [0, 1],
        "Locomotion and Jump"
    );
    assert!(
        crossfades > 20,
        "the run evaluated through cross-fades: {crossfades}"
    );
}

/// Evaluating changes nothing: the animator's state, parameters and pose are the same before
/// and after, and two evaluations agree (the function borrows no world, so it writes no message).
#[test]
fn evaluation_has_no_side_effects() {
    let (mut app, rig) = app();
    for f in 0..70u32 {
        let mut animator = app.world_mut().get_mut::<Animator>(rig).expect("animator");
        drive(&mut animator, f);
        app.update();
    }
    // A pending trigger the pure evaluation fires a transition on, but must not consume.
    app.world_mut()
        .get_mut::<Animator>(rig)
        .expect("animator")
        .trigger(JUMP);
    let world = app.world();
    let animator = world.get::<Animator>(rig).expect("animator");
    let binding = world.get::<SkinnedMesh>(rig).expect("rig");
    let state = capture_state(animator).expect("capture");
    let pose_before = animator.evaluated_pose().clone();
    let params_before = animator.parameters.clone();
    let request = PoseRequest {
        skeleton: &binding.skeleton,
        source: &animator.source,
        state: &state,
        params: &animator.parameters,
        delta: STEP.as_secs_f32(),
        rig,
        socket_targets: None,
    };
    let (mut a, mut b) = (Pose::default(), Pose::default());
    evaluate_pose(&request, &mut a).expect("first");
    evaluate_pose(&request, &mut b).expect("second");
    assert!(same_bits(&a, &b), "the same inputs give the same pose");
    assert_eq!(
        capture_state(animator).expect("again"),
        state,
        "the playback state is untouched"
    );
    assert_eq!(animator.parameters, params_before, "the trigger is not consumed");
    assert!(
        same_bits(animator.evaluated_pose(), &pose_before),
        "the pose buffer is untouched"
    );
}

/// A restored snapshot continues where the snapshot was: a second animator restored from the
/// first's state and given the same parameters evaluates the same pose next frame.
#[test]
fn a_restored_animator_continues_from_the_snapshot() {
    let (mut app, rig) = app();
    for f in 0..50u32 {
        let mut animator = app.world_mut().get_mut::<Animator>(rig).expect("animator");
        drive(&mut animator, f);
        app.update();
    }
    let (skeleton, graph, clips) = demo_net_character().expect("demo rig");
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
    let copy = app
        .world_mut()
        .spawn((Transform::default(), binding, Animator::graph(graph, clips)))
        .id();
    app.update();
    let (state, params) = {
        let animator = app.world().get::<Animator>(rig).expect("animator");
        (
            capture_state(animator).expect("capture"),
            animator.parameters.clone(),
        )
    };
    {
        let mut other = app.world_mut().get_mut::<Animator>(copy).expect("copy");
        restore_state(&mut other, &AnimStateSnapshot::from_words(state.words())).expect("restore");
        other.parameters = params;
    }
    app.update();
    let world = app.world();
    let a = world.get::<Animator>(rig).expect("animator").evaluated_pose();
    let b = world.get::<Animator>(copy).expect("copy").evaluated_pose();
    assert!(same_bits(a, b), "the restored copy shows the original's pose");

    // Ahead: a banked lead joins the next evaluation's delta (one evaluation, not two).
    let (state, params) = {
        let animator = app.world().get::<Animator>(rig).expect("animator");
        (
            capture_state(animator).expect("capture"),
            animator.parameters.clone(),
        )
    };
    let lead = 0.004f32;
    {
        let mut other = app.world_mut().get_mut::<Animator>(copy).expect("copy");
        restore_state_ahead(&mut other, &state, lead).expect("restore ahead");
        other.parameters = params.clone();
    }
    app.update();
    let world = app.world();
    let frame = world.resource::<regia_app::Time>().delta_secs();
    let binding = world.get::<SkinnedMesh>(copy).expect("rig");
    let animator = world.get::<Animator>(copy).expect("copy");
    let mut expected = Pose::default();
    let request = PoseRequest {
        skeleton: &binding.skeleton,
        source: &animator.source,
        state: &state,
        params: &params,
        delta: lead + frame,
        rig: copy,
        socket_targets: None,
    };
    evaluate_pose(&request, &mut expected).expect("pure");
    assert!(
        same_bits(animator.evaluated_pose(), &expected),
        "lead plus frame in one evaluation"
    );
}
