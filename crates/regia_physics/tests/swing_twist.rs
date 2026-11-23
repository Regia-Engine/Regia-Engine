// SPDX-License-Identifier: MPL-2.0
//! `JointKind::SwingTwist` (spec 93 § 2.1): a capsule hanging from a static anchor under a
//! constant torque stops at the range end it is pushed towards, with the documented sign
//! (the body's rotation relative to the connected body, right-hand rule), on every axis;
//! `rest` re-centres the ranges.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, ExternalForce, Joint, JointKind, JointNoContacts, PhysicsPlugin, PhysicsPose,
    RigidBody, ShapeAxis,
};
use regia_scene::Transform;

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// A static anchor at the origin and a capsule hanging below it on a swing-twist joint
/// (twist axis down the capsule, bend axis +X). Gravity is off on the capsule so only the
/// torque acts. The limb weighs about 3.7 kg; torques are chosen for a few rad/s².
fn hanging(app: &mut App, kind: JointKind, torque: Vec3) -> (Entity, Entity) {
    let anchor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::ZERO),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                ..Collider::default()
            },
        ))
        .id();
    let limb = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::ZERO),
            RigidBody {
                gravity_scale: 0.0,
                angular_damping: 2.0,
                ..RigidBody::dynamic()
            },
            Collider {
                shape: ColliderShape::Capsule {
                    half_height: 0.2,
                    radius: 0.05,
                    axis: ShapeAxis::Y,
                },
                offset: Transform::from_translation(Vec3::new(0.0, -0.25, 0.0)),
                // About 3.7 kg: a forearm, so the test torques give human-scale accelerations.
                density: 1000.0,
                ..Collider::default()
            },
            ExternalForce {
                force: Vec3::ZERO,
                torque,
            },
            Joint {
                kind,
                connected: anchor,
                anchor: Vec3::ZERO,
                connected_anchor: Vec3::ZERO,
                break_force: 0.0,
            },
            JointNoContacts,
        ))
        .id();
    (anchor, limb)
}

fn swing_twist(bend: (f32, f32), side: (f32, f32), twist: (f32, f32), rest: Option<Quat>) -> JointKind {
    JointKind::SwingTwist {
        twist_axis: Vec3::NEG_Y,
        bend_axis: Vec3::X,
        bend,
        side,
        twist,
        rest,
    }
}

fn rotation(app: &App, e: Entity) -> Quat {
    app.world().get::<PhysicsPose>(e).map_or_else(
        || app.world().get::<Transform>(e).expect("transform").rotation,
        |p| p.rotation,
    )
}

/// The signed angle of `q` about `axis` (twist decomposition, rad).
fn angle_about(q: Quat, axis: Vec3) -> f32 {
    let q = if q.w < 0.0 { -q } else { q };
    let projected = Vec3::new(q.x, q.y, q.z).dot(axis);
    2.0 * projected.atan2(q.w)
}

fn settle(torque: Vec3, kind: JointKind) -> Quat {
    let mut app = physics_app();
    let (_, limb) = hanging(&mut app, kind, torque);
    for _ in 0..240 {
        app.update();
    }
    rotation(&app, limb)
}

/// The largest |angle| about `axis` the limb reaches over the run.
fn widest(torque: Vec3, kind: JointKind, axis: Vec3) -> f32 {
    let mut app = physics_app();
    let (_, limb) = hanging(&mut app, kind, torque);
    let mut widest = 0.0f32;
    for _ in 0..240 {
        app.update();
        widest = widest.max(angle_about(rotation(&app, limb), axis).abs());
    }
    widest
}

#[test]
fn bend_stops_at_the_range_end_the_torque_pushes_towards() {
    let kind = || swing_twist((-0.2, 0.6), (-0.3, 0.3), (-0.3, 0.3), None);
    // +X torque turns the limb positively about +X: it stops at the bend maximum.
    let up = angle_about(settle(Vec3::X * 2.0, kind()), Vec3::X);
    assert!((up - 0.6).abs() < 0.08, "bend max: {up}");
    // -X torque: the bend minimum.
    let down = angle_about(settle(Vec3::NEG_X * 2.0, kind()), Vec3::X);
    assert!((down + 0.2).abs() < 0.08, "bend min: {down}");
}

#[test]
fn side_and_twist_ranges_hold_with_the_same_sign_convention() {
    // Side axis = twist × bend = (-Y) × X = +Z.
    let side = angle_about(
        settle(
            Vec3::Z * 2.0,
            swing_twist((-0.3, 0.3), (-0.1, 0.4), (-0.3, 0.3), None),
        ),
        Vec3::Z,
    );
    assert!((side - 0.4).abs() < 0.08, "side max: {side}");
    // Twist about the bone (-Y): a torque about -Y turns positively about the twist axis.
    let twist = angle_about(
        settle(
            Vec3::NEG_Y * 0.05,
            swing_twist((-0.3, 0.3), (-0.3, 0.3), (-0.5, 0.25), None),
        ),
        Vec3::NEG_Y,
    );
    assert!((twist - 0.25).abs() < 0.08, "twist max: {twist}");
}

#[test]
fn unlimited_axis_would_run_far_past_the_range() {
    // Control: the same torque on an unlimited ball joint turns the limb far past 0.6 rad
    // (it keeps spinning), so the limits above are what stopped it.
    let free = widest(Vec3::X * 2.0, JointKind::Spherical { cone_limit: None }, Vec3::X);
    assert!(free > 1.5, "unlimited joint turned only {free}");
    // And the limited joint never overshoots its range by much on the way.
    let held = widest(
        Vec3::X * 2.0,
        swing_twist((-0.2, 0.6), (-0.3, 0.3), (-0.3, 0.3), None),
        Vec3::X,
    );
    assert!(held < 0.7, "the limited joint overshot to {held}");
}

#[test]
fn rest_recentres_the_ranges() {
    // Rest 0.5 rad about +X with a ±0.1 bend range: the limb starts outside the range (at 0)
    // and is driven into [0.4, 0.6]; a +X torque stops it at 0.6.
    let rest = Some(Quat::from_rotation_x(0.5));
    let idle = angle_about(
        settle(
            Vec3::ZERO,
            swing_twist((-0.1, 0.1), (-0.1, 0.1), (-0.1, 0.1), rest),
        ),
        Vec3::X,
    );
    assert!(
        (0.35..=0.65).contains(&idle),
        "idle limb not pulled into the range: {idle}"
    );
    let pushed = angle_about(
        settle(
            Vec3::X * 2.0,
            swing_twist((-0.1, 0.1), (-0.1, 0.1), (-0.1, 0.1), rest),
        ),
        Vec3::X,
    );
    assert!((pushed - 0.6).abs() < 0.08, "re-centred max: {pushed}");
}

/// `settle` with the limb created already turned by `start` (before the first step builds the
/// joint).
fn settle_from(start: Quat, torque: Vec3, kind: JointKind) -> Quat {
    let mut app = physics_app();
    let (_, limb) = hanging(&mut app, kind, torque);
    app.world_mut()
        .get_mut::<Transform>(limb)
        .expect("transform")
        .rotation = start;
    for _ in 0..240 {
        app.update();
    }
    rotation(&app, limb)
}

#[test]
fn rest_none_takes_the_relative_rotation_at_creation() {
    // Spec 93 § 2.1: `rest: None` measures the angles from the relative rotation the bodies
    // have when the joint is created. The limb starts 0.5 rad about +X with a ±0.1 range: it
    // stays near 0.5 at rest, and a +X torque stops it at 0.6, not at 0.1.
    let start = Quat::from_rotation_x(0.5);
    let range = || swing_twist((-0.1, 0.1), (-0.1, 0.1), (-0.1, 0.1), None);
    let idle = angle_about(settle_from(start, Vec3::ZERO, range()), Vec3::X);
    assert!(
        (0.4..=0.6).contains(&idle),
        "idle limb left its creation pose: {idle}"
    );
    let pushed = angle_about(settle_from(start, Vec3::X * 2.0, range()), Vec3::X);
    assert!(
        (pushed - 0.6).abs() < 0.08,
        "range end from the creation pose: {pushed}"
    );
    // Control: an explicit identity rest from the same start pulls it back into [-0.1, 0.1].
    let identity = swing_twist((-0.1, 0.1), (-0.1, 0.1), (-0.1, 0.1), Some(Quat::IDENTITY));
    let pulled = angle_about(settle_from(start, Vec3::ZERO, identity), Vec3::X);
    assert!(
        (-0.15..=0.15).contains(&pulled),
        "identity rest pulls it back: {pulled}"
    );
}
