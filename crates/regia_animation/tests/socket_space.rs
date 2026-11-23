// SPDX-License-Identifier: MPL-2.0
//! Socket spaces (deep-zai-animation finding 1): [`SocketTargets`] holds rig object-space
//! positions (what the IK nodes solve in), a socket entity's `Transform` lands on the joint's
//! world point however the rig and the socket entity are parented.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::components::spawn_skeleton;
use regia_animation::graph::{ClipNode, TwoBoneIKNode};
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_app::prelude::*;
use regia_core::math::{Affine3A, Mat4, Vec3};
use regia_scene::{GlobalTransform, Transform};

fn at(x: f32, y: f32, z: f32) -> LocalBind {
    LocalBind {
        translation: Vec3::new(x, y, z),
        ..LocalBind::default()
    }
}

/// root → shoulder → elbow → hand (a slightly bent arm), plus `aim` hanging off the root: the
/// joint the IK target socket sits on, outside the chain so the solve never moves it.
fn arm() -> Skeleton {
    Skeleton::from_local_bind(
        vec![
            "root".into(),
            "shoulder".into(),
            "elbow".into(),
            "hand".into(),
            "aim".into(),
        ],
        vec![None, Some(0), Some(1), Some(2), Some(0)],
        vec![
            LocalBind::default(),
            at(0.0, 1.0, 0.0),
            at(0.05, -0.5, 0.0),
            at(0.0, -0.5, 0.0),
            at(0.4, 0.6, 0.3),
        ],
    )
    .expect("valid skeleton")
}

/// The socket's offset from the `aim` joint; the IK target is `aim` + this, in object space.
const OFFSET: Vec3 = Vec3::new(0.0, 0.0, 0.1);
/// Where the `grab` socket sits in the rig's object space.
const TARGET: Vec3 = Vec3::new(0.4, 0.6, 0.4);

fn ik_graph() -> AnimGraph {
    AnimGraph {
        parameters: Vec::new(),
        root: AnimNode::TwoBoneIK(TwoBoneIKNode {
            inner: Box::new(AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            })),
            chain: ["shoulder".into(), "elbow".into(), "hand".into()],
            target: IkTarget::Socket("grab".into()),
            pole: None,
            weight_param: None,
        }),
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        Duration::from_millis(16),
    ));
    app.update();
    app
}

/// A rig at `transform` running the socket-targeted two-bone IK graph, with its `grab` socket.
fn spawn_ik_rig(app: &mut App, transform: Transform) -> (Entity, Entity) {
    let mut clips = ClipLibrary::new();
    clips.insert(AnimationClip::empty("idle", 1.0));
    let binding = spawn_skeleton(app.world_mut(), Arc::new(arm()));
    let rig = app
        .world_mut()
        .spawn((transform, binding, Animator::graph(ik_graph(), clips)))
        .id();
    let socket = app
        .world_mut()
        .spawn((
            Transform::default(),
            Socket {
                rig,
                name: "grab".into(),
                joint: "aim".into(),
                offset: Transform::from_translation(OFFSET),
            },
        ))
        .id();
    (rig, socket)
}

fn globals(app: &App, rig: Entity) -> Vec<Mat4> {
    app.world()
        .get::<JointGlobals>(rig)
        .expect("joint globals published")
        .globals
        .clone()
}

fn hand(app: &App, rig: Entity) -> Vec3 {
    globals(app, rig)[3].w_axis.truncate()
}

#[test]
fn socket_ik_reaches_the_same_pose_wherever_the_rig_stands() {
    let mut app = app();
    let (home, _) = spawn_ik_rig(&mut app, Transform::default());
    let (away, away_socket) = spawn_ik_rig(&mut app, Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)));
    // Sockets resolve after animation applies, IK reads them on the next evaluation.
    for _ in 0..4 {
        app.update();
    }
    let targets = &app.world().resource::<SocketTargets>().0;
    let away_target = targets.get(&(away, "grab".to_string())).copied();
    assert!(
        away_target.is_some_and(|t| t.abs_diff_eq(TARGET, 1e-5)),
        "the socket map holds the rig's object space: {away_target:?}, want {TARGET}"
    );
    // The IK really solves (not a vacuous comparison of two untouched bind poses).
    let reached = hand(&app, home);
    assert!(
        reached.abs_diff_eq(TARGET, 1e-3),
        "the rig at the origin puts its hand on the socket: {reached}"
    );
    let (home_pose, away_pose) = (globals(&app, home), globals(&app, away));
    for (joint, (a, b)) in home_pose.iter().zip(&away_pose).enumerate() {
        assert!(
            a.abs_diff_eq(*b, 1e-4),
            "joint {joint}: a rig at (100, 0, 0) reaches another pose than the same rig at the \
             origin\nhome {a:?}\naway {b:?}"
        );
    }
    // The socket entity itself still lands on the world point.
    let world = app
        .world()
        .get::<Transform>(away_socket)
        .expect("socket")
        .translation;
    assert!(
        world.abs_diff_eq(TARGET + Vec3::new(100.0, 0.0, 0.0), 1e-4),
        "{world}"
    );
}

/// A rig parented under a moved entity: its world placement is its `GlobalTransform`, not its
/// local `Transform`.
#[test]
fn a_parented_rigs_socket_lands_on_the_world_point() {
    let mut app = app();
    let parent = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)))
        .id();
    let (rig, socket) = spawn_ik_rig(&mut app, Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)));
    // The rig's propagated world placement: parent (50, 0, 0) · local (1, 0, 0).
    app.world_mut().entity_mut(rig).insert((
        ChildOf(parent),
        GlobalTransform(Affine3A::from_translation(Vec3::new(51.0, 0.0, 0.0))),
    ));
    for _ in 0..4 {
        app.update();
    }
    let world = app.world().get::<Transform>(socket).expect("socket").translation;
    let want = TARGET + Vec3::new(51.0, 0.0, 0.0);
    assert!(
        world.abs_diff_eq(want, 1e-4),
        "the socket of a parented rig sits at {world}, want {want}"
    );
    let target = app
        .world()
        .resource::<SocketTargets>()
        .0
        .get(&(rig, "grab".to_string()))
        .copied();
    assert!(
        target.is_some_and(|t| t.abs_diff_eq(TARGET, 1e-5)),
        "the IK target stays in object space: {target:?}"
    );
}

/// A socket entity parented to its rig (the natural attachment): its local `Transform` is the
/// joint offset in the rig's space, so propagation does not add the rig's transform twice.
#[test]
fn a_socket_entity_parented_to_its_rig_is_not_transformed_twice() {
    let mut app = app();
    let (rig, socket) = spawn_ik_rig(&mut app, Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)));
    app.world_mut().entity_mut(socket).insert(ChildOf(rig));
    for _ in 0..4 {
        app.update();
    }
    let local = app.world().get::<Transform>(socket).expect("socket").translation;
    assert!(
        local.abs_diff_eq(TARGET, 1e-4),
        "the rig-child socket's local transform is {local}, want {TARGET} (rig space)"
    );
}

/// A socket entity under some other moved entity gets the world point expressed in that
/// parent's space.
#[test]
fn a_socket_entity_under_another_parent_is_expressed_in_that_parent() {
    let mut app = app();
    let (_rig, socket) = spawn_ik_rig(&mut app, Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)));
    let holder = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            GlobalTransform(Affine3A::from_translation(Vec3::new(0.0, 5.0, 0.0))),
        ))
        .id();
    app.world_mut().entity_mut(socket).insert(ChildOf(holder));
    for _ in 0..4 {
        app.update();
    }
    let local = app.world().get::<Transform>(socket).expect("socket").translation;
    let want = TARGET + Vec3::new(10.0, -5.0, 0.0);
    assert!(local.abs_diff_eq(want, 1e-4), "local {local}, want {want}");
}
