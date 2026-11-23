// SPDX-License-Identifier: MPL-2.0
//! Animation robustness through `AnimationPlugin` and the public API only (ADR-871): a looping
//! turn's root motion keeps turning the same way across loop points, forwards and in reverse,
//! and a NaN graph parameter never reaches the joints or the skin palette.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{AnimationClip, JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::graph::AnimGraph;
use regia_animation::pose::translated;
use regia_animation::{AnimationPlugin, Animator, ClipLibrary, RootMotionOutput, Skeleton, SkinPalette};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;

const DT: f32 = 1.0 / 30.0;
const QUARTER: f32 = std::f32::consts::FRAC_PI_2;

/// The rig root turns 90° about +Y per 1 s lap, extracted as yaw root motion.
fn turning_clip() -> AnimationClip {
    let mut clip = AnimationClip::empty("turn", 1.0);
    let times: Vec<f32> = (0..=40).map(|k| k as f32 / 40.0).collect();
    clip.tracks.push(JointTrack {
        joint: 0,
        translations: TrackCurve::Static(Vec3::ZERO),
        rotations: TrackCurve::Animated {
            values: times.iter().map(|t| Quat::from_rotation_y(QUARTER * t)).collect(),
            times,
            exact_rotation: true,
        },
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: RootMotionMode::Yaw,
    });
    clip
}

fn app_with(animator: Animator) -> (App, Entity, Vec<Entity>) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f32(DT),
    ));
    let skeleton = Arc::new(
        Skeleton::from_local_bind(
            vec!["root".into(), "child".into()],
            vec![None, Some(0)],
            vec![translated(Vec3::ZERO), translated(Vec3::Y)],
        )
        .expect("valid skeleton"),
    );
    let rig = app.world_mut().spawn(Transform::default()).id();
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let joints = binding.joints.clone();
    app.world_mut().entity_mut(rig).insert((binding, animator));
    (app, rig, joints)
}

/// Run `frames` updates, draining the root motion after each; returns each frame's yaw.
fn yaw_per_frame(app: &mut App, rig: Entity, frames: usize) -> Vec<f32> {
    let mut yaws = Vec::with_capacity(frames);
    for _ in 0..frames {
        app.update();
        let yaw = app
            .world_mut()
            .get_mut::<RootMotionOutput>(rig)
            .map_or(0.0, |mut out| {
                out.take().1.to_euler(regia_core::glam::EulerRot::YXZ).0
            });
        yaws.push(yaw);
    }
    yaws
}

#[test]
fn a_looping_turn_keeps_turning_across_loop_points() {
    for speed in [1.0f32, -1.0] {
        let mut animator = Animator::single(turning_clip(), true);
        animator.speed = speed;
        let (mut app, rig, _) = app_with(animator);
        // 2.5 s: two loop points (two wraps forwards, two backwards).
        let yaws = yaw_per_frame(&mut app, rig, 75);
        let step = QUARTER * DT * speed;
        let moving: Vec<f32> = yaws.iter().copied().filter(|y| y.abs() > 1e-6).collect();
        assert!(moving.len() >= 70, "speed {speed}: {} frames moved", moving.len());
        for (frame, yaw) in moving.iter().enumerate() {
            // Every frame turns by the same small step. The old wrap composition swung back by
            // most of a lap (≈ −1.5 rad) at each loop point.
            assert!(
                (yaw - step).abs() < 0.1 * step.abs(),
                "speed {speed}, moving frame {frame}: yaw {yaw}, expected {step}"
            );
        }
    }
}

#[test]
fn a_nan_parameter_never_reaches_the_joints() {
    let graph = AnimGraph::from_ron_str(
        r#"(format_version: 2, parameters: [("W", Float(0.5))],
            root: Blend((
                a: Clip((clip: "A", looping: true, speed: 1.0, sync_group: None)),
                b: Clip((clip: "B", looping: true, speed: 1.0, sync_group: None)),
                alpha_param: "W")))"#,
    )
    .expect("graph parses");
    let lift = |name: &str, height: f32| {
        let mut clip = AnimationClip::empty(name, 1.0);
        clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Static(Vec3::new(0.0, height, 0.0)),
            rotations: TrackCurve::Static(Quat::from_rotation_x(height)),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip
    };
    let mut clips = ClipLibrary::new();
    clips.insert(lift("A", 1.0));
    clips.insert(lift("B", 2.0));
    let mut animator = Animator::graph(graph, clips);
    animator.set("W", f32::NAN);
    let (mut app, rig, joints) = app_with(animator);
    for _ in 0..5 {
        app.update();
    }
    for joint in joints {
        let t = app.world().get::<Transform>(joint).copied().unwrap_or_default();
        assert!(
            t.translation.is_finite() && t.rotation.is_finite() && t.scale.is_finite(),
            "{t:?}"
        );
    }
    // NaN reads as weight 0: the pose is clip A's.
    let child = app
        .world()
        .get::<Transform>(app_child(&app, rig))
        .copied()
        .unwrap_or_default();
    assert!((child.translation.y - 1.0).abs() < 1e-4, "{child:?}");
    let palette = app.world().get::<SkinPalette>(rig);
    assert!(palette.is_some_and(|p| p.matrices.iter().all(|m| m.is_finite())));
}

/// The child joint entity of the rig (joint 1).
fn app_child(app: &App, rig: Entity) -> Entity {
    app.world()
        .get::<regia_animation::SkinnedMesh>(rig)
        .and_then(|mesh| mesh.joints.get(1).copied())
        .unwrap_or(rig)
}

#[test]
fn reverse_root_motion_reaches_the_character_controller() {
    // A consumer such as `regia_framework`'s `Character` skips pending motion that covers no
    // time. Reverse playback recorded 0 s, so its root motion never moved a character.
    let mut animator = Animator::single(turning_clip(), true);
    animator.speed = -1.0;
    let (mut app, rig, _) = app_with(animator);
    for _ in 0..10 {
        app.update();
    }
    let (_, rotation, seconds) = app
        .world_mut()
        .get_mut::<RootMotionOutput>(rig)
        .map(|mut out| out.take())
        .expect("root motion output");
    assert!(seconds > 0.25, "{seconds} s of reverse playback");
    let yaw = rotation.to_euler(regia_core::glam::EulerRot::YXZ).0;
    assert!(
        (yaw + QUARTER * seconds).abs() < 0.01,
        "yaw {yaw} over {seconds} s"
    );
}

#[test]
fn a_nan_speed_freezes_the_clip_instead_of_poisoning_root_motion() {
    let mut animator = Animator::single(turning_clip(), true);
    animator.speed = f32::NAN;
    let (mut app, rig, joints) = app_with(animator);
    for _ in 0..5 {
        app.update();
    }
    let (translation, rotation, seconds) = app
        .world_mut()
        .get_mut::<RootMotionOutput>(rig)
        .map(|mut out| out.take())
        .expect("root motion output");
    assert!(translation.is_finite() && rotation.is_finite() && seconds.is_finite());
    assert_eq!(seconds, 0.0, "a NaN speed covers no animation time");
    for joint in joints {
        let t = app.world().get::<Transform>(joint).copied().unwrap_or_default();
        assert!(t.rotation.is_finite(), "{t:?}");
    }
}
