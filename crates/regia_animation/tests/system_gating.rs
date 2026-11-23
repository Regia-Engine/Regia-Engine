// SPDX-License-Identifier: MPL-2.0
//! Animator update gating (`WhenVisible`, `Lod`), notify messages and socket placement through
//! the animation plugin, driven by a fixed manual time step.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::{GlobalTransform, Transform};

/// Every `AnimNotify` seen so far, in order.
#[derive(Resource, Default)]
struct Seen(Vec<AnimNotify>);

fn collect(mut reader: MessageReader<AnimNotify>, mut seen: ResMut<Seen>) {
    seen.0.extend(reader.read().cloned());
}

fn skeleton() -> Skeleton {
    let at = |y: f32| LocalBind {
        translation: Vec3::new(0.0, y, 0.0),
        ..LocalBind::default()
    };
    Skeleton::from_local_bind(
        vec!["root".into(), "child".into()],
        vec![None, Some(0)],
        vec![at(0.0), at(1.0)],
    )
    .expect("valid skeleton")
}

fn notify(time: f32, name: &str, payload: Option<&str>) -> Notify {
    Notify {
        time,
        name: name.into(),
        payload: payload.map(String::from),
    }
}

/// An app with a 0.1 s manual step, one warm-up update (the first frame has no delta) and a rig
/// playing a looping 10 s clip with the given notifies.
fn setup(notifies: Vec<Notify>, mode: UpdateMode) -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(100),
    ));
    app.init_resource::<Seen>();
    // Ordered after evaluation so a notify written this frame is read this frame; unordered, the
    // executor may run `collect` first and the message only arrives on the next update.
    app.add_systems(
        Update,
        collect.after(regia_animation::systems::evaluate_animators),
    );
    app.update();
    let mut clip = AnimationClip::empty("clip", 10.0);
    clip.notifies = notifies;
    let mut animator = Animator::single(clip, true);
    animator.update_mode = mode;
    let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton()));
    let rig = app
        .world_mut()
        .spawn((Transform::default(), binding, animator))
        .id();
    (app, rig)
}

fn seen(app: &App) -> Vec<(String, Option<String>)> {
    app.world()
        .resource::<Seen>()
        .0
        .iter()
        .map(|n| (n.name.clone(), n.payload.clone()))
        .collect()
}

#[test]
fn when_visible_does_not_advance_until_marked() {
    let (mut app, rig) = setup(vec![notify(0.05, "tick", None)], UpdateMode::WhenVisible);
    app.update();
    app.update();
    assert!(seen(&app).is_empty(), "an unmarked animator must not advance");
    app.world_mut().entity_mut(rig).insert(AnimVisible);
    app.update();
    assert_eq!(seen(&app).len(), 1, "one visible step crosses the notify");
}

#[test]
fn lod_far_rate_steps_once_every_four_frames() {
    // Notifies at 0.05 (inside one 0.1 s step) and 0.25 (only reached by a 0.4 s step).
    let (mut app, _rig) = setup(
        vec![notify(0.05, "a", None), notify(0.25, "b", None)],
        UpdateMode::Lod {
            near: 1.0,
            mid: 2.0,
            far_rate: 0.25,
        },
    );
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(10.0, 0.0, 0.0),
    });
    for _ in 0..3 {
        app.update();
    }
    assert!(seen(&app).is_empty(), "far animators skip three of four frames");
    app.update();
    let names: Vec<String> = seen(&app).into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        ["a"],
        "the fourth frame steps one frame of clip time, not four"
    );
}

#[test]
fn lod_distance_uses_world_position_for_a_parented_rig() {
    // Local translation 0, but the parent puts the rig 100 m away: the camera there means
    // "near", so the first step (full rate) crosses the notify.
    let (mut app, rig) = setup(
        vec![notify(0.05, "a", None)],
        UpdateMode::Lod {
            near: 1.0,
            mid: 2.0,
            far_rate: 0.25,
        },
    );
    let parent = app.world_mut().spawn(Transform::default()).id();
    app.world_mut().entity_mut(rig).insert((
        ChildOf(parent),
        GlobalTransform(regia_core::math::Affine3A::from_translation(Vec3::new(
            100.0, 0.0, 0.0,
        ))),
    ));
    app.insert_resource(AnimLodCamera {
        position: Vec3::new(100.0, 0.0, 0.0),
    });
    app.update();
    assert_eq!(seen(&app).len(), 1, "near in world space, so full rate");
}

#[test]
fn notify_is_an_ecs_message_with_payload() {
    let (mut app, rig) = setup(vec![notify(0.05, "Footstep", Some("L"))], UpdateMode::Always);
    app.update();
    let messages = &app.world().resource::<Seen>().0;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].entity, rig);
    assert_eq!(messages[0].name, "Footstep");
    assert_eq!(messages[0].payload.as_deref(), Some("L"));
}

#[test]
fn resolve_sockets_is_rig_times_joint_times_offset() {
    let (mut app, rig) = setup(Vec::new(), UpdateMode::Always);
    app.world_mut()
        .entity_mut(rig)
        .insert(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)));
    let socket = app
        .world_mut()
        .spawn((
            Transform::default(),
            Socket {
                rig,
                name: "hand".into(),
                joint: "child".into(),
                offset: Transform::from_translation(Vec3::new(0.0, 0.0, 0.5)),
            },
        ))
        .id();
    let missing = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::splat(7.0)),
            Socket {
                rig,
                name: "ghost".into(),
                joint: "no_such_joint".into(),
                offset: Transform::default(),
            },
        ))
        .id();
    app.update();
    app.update();
    let t = app.world().get::<Transform>(socket).expect("socket").translation;
    assert!(t.abs_diff_eq(Vec3::new(10.0, 1.0, 0.5), 1e-5), "{t:?}");
    let t = app.world().get::<Transform>(missing).expect("socket").translation;
    assert_eq!(t, Vec3::splat(7.0), "a missing joint leaves the transform alone");
}

#[test]
fn a_socket_on_a_joint_past_a_stale_palette_is_skipped() {
    let (mut app, rig) = setup(Vec::new(), UpdateMode::Always);
    app.update();
    app.update();
    // The palette still has two joints; the skeleton gains a third (a hot reload while the
    // rig's evaluation is gated off), and a socket points at it.
    let longer = Skeleton::from_local_bind(
        vec!["root".into(), "child".into(), "tip".into()],
        vec![None, Some(0), Some(1)],
        vec![LocalBind::default(); 3],
    )
    .expect("valid skeleton");
    let stale = app
        .world()
        .get::<SkinnedMesh>(rig)
        .expect("binding")
        .joints
        .clone();
    app.world_mut().entity_mut(rig).insert(SkinnedMesh {
        skeleton: Arc::new(longer),
        joints: stale,
    });
    app.world_mut().entity_mut(rig).remove::<Animator>();
    let socket = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::splat(3.0)),
            Socket {
                rig,
                name: "tip".into(),
                joint: "tip".into(),
                offset: Transform::default(),
            },
        ))
        .id();
    app.update();
    let t = app.world().get::<Transform>(socket).expect("socket").translation;
    assert_eq!(
        t,
        Vec3::splat(3.0),
        "an out-of-range joint leaves the transform alone"
    );
}
