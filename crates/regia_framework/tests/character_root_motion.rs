// SPDX-License-Identifier: MPL-2.0
//! Root motion consumed by `Character` (specs/17 § 4): an animator's root-motion clip moves
//! the character through the rapier character controller, so a wall stops it; a yaw clip
//! turns it; `RootMotionApply::Off` ignores the animation.

use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{JointTrack, RootMotionMode, RootMotionTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::skeleton::LocalBind;
use regia_animation::{AnimationClip, AnimationPlugin, Animator, Skeleton};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState, RootMotionApply,
    RootMotionDriven, use_rapier_character_controller,
};
use regia_physics::{CharacterBody, Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

const DT: f64 = 1.0 / 62.5;
const REST_Y: f32 = 1.2;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins((CharacterPlugin, AnimationPlugin));
    use_rapier_character_controller(&mut app);
    app
}

fn static_box(app: &mut App, half: Vec3, translation: Vec3) {
    app.world_mut().spawn((
        Transform::from_translation(translation),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box { half_extents: half },
            ..Collider::default()
        },
    ));
}

/// A one-joint rig whose clip walks the root 1.5 m/s towards −Z (XZ root motion), or turns
/// it 90°/s (yaw root motion).
fn clip(yaw: bool) -> AnimationClip {
    clip_speed(yaw, 1.5)
}

/// A one-joint rig whose clip drives the root at `speed` m/s towards −Z (XZ root motion), or
/// turns it 90°/s (yaw root motion).
fn clip_speed(yaw: bool, speed: f32) -> AnimationClip {
    let mut clip = AnimationClip::empty("loco", 1.0);
    let (translations, rotations) = if yaw {
        (
            TrackCurve::Static(Vec3::ZERO),
            TrackCurve::Animated {
                times: vec![0.0, 0.5, 1.0],
                values: vec![
                    Quat::IDENTITY,
                    Quat::from_rotation_y(std::f32::consts::FRAC_PI_4),
                    Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                ],
                exact_rotation: true,
            },
        )
    } else {
        (
            TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::ZERO, Vec3::new(0.0, 0.0, -speed)],
                exact_rotation: false,
            },
            TrackCurve::Static(Quat::IDENTITY),
        )
    };
    clip.tracks.push(JointTrack {
        joint: 0,
        translations,
        rotations,
        scales: TrackCurve::Static(Vec3::ONE),
    });
    clip.root_motion = Some(RootMotionTrack {
        joint: 0,
        mode: if yaw {
            RootMotionMode::Yaw
        } else {
            RootMotionMode::XZ
        },
    });
    clip
}

fn spawn_character(app: &mut App, start: Vec3, yaw: bool, mode: RootMotionApply) -> Entity {
    spawn_with_clip(app, start, clip(yaw), mode)
}

fn spawn_with_clip(app: &mut App, start: Vec3, clip: AnimationClip, mode: RootMotionApply) -> Entity {
    let skeleton = Arc::new(
        Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton"),
    );
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    app.world_mut()
        .spawn((
            Transform::from_translation(start),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState::default(),
            CharacterBody::default(),
            binding,
            Animator::single(clip, true),
            RootMotionDriven {
                mode,
                ..Default::default()
            },
        ))
        .id()
}

fn transform(app: &App, e: Entity) -> Transform {
    app.world().get::<Transform>(e).copied().expect("transform")
}

fn run(app: &mut App, frames: usize) {
    for _ in 0..frames {
        app.update();
    }
}

#[test]
fn root_motion_walks_the_character_and_a_wall_stops_it() {
    let mut app = app();
    static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
    // Wall face at z = −4.
    static_box(&mut app, Vec3::new(5.0, 2.0, 0.5), Vec3::new(0.0, 2.0, -4.5));
    let e = spawn_character(
        &mut app,
        Vec3::new(0.0, REST_Y, 0.0),
        false,
        RootMotionApply::Horizontal,
    );
    run(&mut app, 10);
    let start = transform(&app, e).translation;
    // One second of animation (62.5 frames): 1.5 m with no input at all.
    run(&mut app, 63);
    let after = transform(&app, e).translation;
    let travelled = start.z - after.z;
    assert!((travelled - 1.5).abs() < 0.1, "traveled {travelled} m in 1 s");
    assert!(after.x.abs() < 1e-3, "straight line: {after}");
    let velocity = app.world().get::<CharacterState>(e).expect("state").velocity;
    assert!((velocity.z + 1.5).abs() < 0.15, "state velocity {velocity}");
    assert!(app.world().get::<CharacterState>(e).expect("state").grounded);
    // Keep walking: the wall stops the capsule (radius 0.3) at its face.
    run(&mut app, 250);
    let blocked = transform(&app, e).translation;
    assert!(
        blocked.z > -4.0 + 0.25,
        "the wall stops root motion: z {}",
        blocked.z
    );
    assert!(blocked.z < -3.5, "walked up to the wall: z {}", blocked.z);
    assert!((blocked.y - REST_Y).abs() < 0.1, "stays on the ground: {blocked}");
}

#[test]
fn yaw_root_motion_turns_the_character() {
    let mut app = app();
    static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
    let e = spawn_character(
        &mut app,
        Vec3::new(0.0, REST_Y, 0.0),
        true,
        RootMotionApply::Horizontal,
    );
    run(&mut app, 10);
    let (yaw0, _, _) = transform(&app, e)
        .rotation
        .to_euler(regia_core::glam::EulerRot::YXZ);
    run(&mut app, 31); // ~0.5 s of animation: ~45°
    let (yaw1, _, _) = transform(&app, e)
        .rotation
        .to_euler(regia_core::glam::EulerRot::YXZ);
    let turned = yaw1 - yaw0;
    assert!(
        (turned - std::f32::consts::FRAC_PI_4).abs() < 0.06,
        "turned {turned} rad in 0.5 s"
    );
    // The rig's own root joint stays pinned (the turn moved the character, not the pose).
    let pose_root = app
        .world()
        .get::<Animator>(e)
        .expect("animator")
        .evaluated_pose()
        .rotations[0];
    assert!(pose_root.angle_between(Quat::IDENTITY) < 1e-3);
}

#[test]
fn root_motion_off_leaves_the_character_to_input() {
    let mut app = app();
    static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
    let e = spawn_character(&mut app, Vec3::new(0.0, REST_Y, 0.0), false, RootMotionApply::Off);
    run(&mut app, 70);
    let at = transform(&app, e).translation;
    assert!(at.z.abs() < 1e-3, "no input, root motion off: stays put ({at})");
}

/// Audit v033b (High): yaw root motion read from a rig scaled to zero decomposed a NaN rotation,
/// and the character's `Transform.rotation` became NaN for good. It stays finite, and the turn
/// resumes once the rig's scale is back.
#[test]
fn a_rig_scaled_to_zero_never_turns_the_character_to_nan() {
    let mut app = app();
    static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
    let skeleton = Arc::new(
        Skeleton::from_local_bind(vec!["root".into()], vec![None], vec![LocalBind::default()])
            .expect("skeleton"),
    );
    let binding = spawn_skeleton(app.world_mut(), skeleton);
    let rig = app
        .world_mut()
        .spawn((
            Transform {
                scale: Vec3::ZERO,
                ..Transform::IDENTITY
            },
            // This app propagates no transforms: the rig's global matrix is set as a propagation
            // would leave it.
            regia_scene::GlobalTransform(regia_core::math::Affine3A::from_scale(Vec3::ZERO)),
            binding,
            Animator::single(clip(true), true),
        ))
        .id();
    let e = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, REST_Y, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState::default(),
            CharacterBody::default(),
            RootMotionDriven {
                rig: Some(rig),
                ..Default::default()
            },
        ))
        .id();
    run(&mut app, 20);
    let rotation = transform(&app, e).rotation;
    assert!(rotation.is_finite(), "{rotation:?}");
    app.world_mut().get_mut::<Transform>(rig).expect("rig").scale = Vec3::ONE;
    app.world_mut()
        .entity_mut(rig)
        .insert(regia_scene::GlobalTransform(regia_core::math::Affine3A::IDENTITY));
    run(&mut app, 31);
    let rotation = transform(&app, e).rotation;
    assert!(rotation.is_finite(), "{rotation:?}");
    assert!(
        rotation.angle_between(Quat::IDENTITY) > 0.1,
        "the turn resumes once the rig has a scale: {rotation:?}"
    );
}

/// audit-v040 H1 / fixrev-06-k H1: root motion banked across a frame hitch must not sweep in
/// one `move_shape` call. The bank drains in fixed-step quanta with a sub-step dt, so the
/// same animation time given as twenty 1/50 s frames or as one 0.1 s frame plus fifteen
/// 1/50 s frames performs the same sequence of fixed-step-sized sweeps and the wall stops
/// both runs at the same contact; before the audit-v040 fix the hitched run swept 1 m in one
/// call and tunneled through the 0.1 m wall slab.
#[test]
fn hitched_root_motion_sweeps_in_fixed_quanta_and_matches_steady_pacing() {
    const HZ: f64 = 50.0;
    let mk_app = || {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / HZ,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = HZ;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((CharacterPlugin, AnimationPlugin));
        use_rapier_character_controller(&mut app);
        // Ground and a thin wall whose face is at z = -0.5.
        static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
        static_box(&mut app, Vec3::new(5.0, 2.0, 0.05), Vec3::new(0.0, 2.0, -0.55));
        app
    };
    // 10 m/s: a 0.1 s frame banks 1 m of root motion.
    let walk = |app: &mut App| {
        spawn_with_clip(
            app,
            Vec3::new(0.0, REST_Y, 0.0),
            clip_speed(false, 10.0),
            RootMotionApply::Horizontal,
        )
    };

    // Steady pacing: 20 frames of 1/50 s = 0.4 s of animation (4 m unblocked).
    let mut steady = mk_app();
    let e_steady = walk(&mut steady);
    run(&mut steady, 20);

    // Hitched pacing: one 0.1 s frame (5 steps' worth of wall time, banking 1 m) then fifteen
    // 1/50 s frames: the same 0.4 s of animation.
    let mut hitched = mk_app();
    let e_hitched = walk(&mut hitched);
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(0.1)));
    hitched.update();
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / HZ,
    )));
    run(&mut hitched, 15);

    let z_steady = transform(&steady, e_steady).translation.z;
    let z_hitched = transform(&hitched, e_hitched).translation.z;
    assert!(
        z_steady > -0.35,
        "steady pacing stops at the wall contact, got z {z_steady}"
    );
    assert!(
        z_hitched > -0.35,
        "the hitched run did not tunnel through the wall: z {z_hitched}"
    );
    assert!(
        (z_steady - z_hitched).abs() < 1e-3,
        "both pacings resolve the wall identically: steady {z_steady}, hitched {z_hitched}"
    );
}

/// A platform marker for the fixed-step kinematic drive below.
#[derive(regia_ecs::prelude::Component)]
struct SlidingPlatform;

/// Advances the sliding platform one fixed step (runs before the physics chain reads kinematic
/// poses).
fn advance_sliding_platform(
    mut platforms: Query<&mut Transform, With<SlidingPlatform>>,
    fixed: Res<FixedTime>,
    speed: Res<PlatformSpeed>,
) {
    let dt = (1.0 / fixed.hz) as f32;
    for mut t in &mut platforms {
        t.translation.x += speed.0 * dt;
    }
}

/// The sliding speed of [`SlidingPlatform`], in m/s.
#[derive(regia_ecs::prelude::Resource)]
struct PlatformSpeed(f32);

/// A kinematic ground sliding +X at `speed`, advanced one fixed step per fixed step so steady
/// and hitched pacings drive the identical platform trajectory.
fn sliding_platform(app: &mut App, speed: f32) -> Entity {
    let platform = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(8.0, 0.5, 8.0),
                },
                ..Collider::default()
            },
            SlidingPlatform,
        ))
        .id();
    app.insert_resource(PlatformSpeed(speed));
    app.add_systems(
        FixedUpdate,
        advance_sliding_platform.before(regia_physics::PhysicsStep),
    );
    platform
}

/// fixrev-06-k H1 (moving-platform variant): the backend adds the platform's motion to the
/// desired move once per `move_shape` call. When a hitched bank drained in several quantum
/// `move_shape` calls inside one fixed step, the character was carried `quanta × platform
/// velocity × dt` in that step alone and then carried again by the remaining fixed steps of
/// the frame — ending ahead of the steady-pacing run. Draining at most one fixed step of root
/// motion per `step_character` keeps one carry per fixed step: both pacings ride the platform
/// to the same position.
#[test]
fn hitched_root_motion_on_a_moving_platform_matches_steady_pacing() {
    const HZ: f64 = 50.0;
    let mk_app = || {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / HZ,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = HZ;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((CharacterPlugin, AnimationPlugin));
        use_rapier_character_controller(&mut app);
        // A 2 m/s platform the character root-motion-walks across (−Z at 10 m/s).
        sliding_platform(&mut app, 2.0);
        app
    };
    let walk = |app: &mut App| {
        spawn_with_clip(
            app,
            Vec3::new(0.0, REST_Y, 0.0),
            clip_speed(false, 10.0),
            RootMotionApply::Horizontal,
        )
    };

    // Steady: 20 frames of 1/50 s = 0.4 s of animation (4 m of −Z travel).
    let mut steady = mk_app();
    let e_steady = walk(&mut steady);
    run(&mut steady, 20);

    // Hitched: one 0.1 s frame (banking 1 m of root motion) then fifteen 1/50 s frames.
    let mut hitched = mk_app();
    let e_hitched = walk(&mut hitched);
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(0.1)));
    hitched.update();
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / HZ,
    )));
    run(&mut hitched, 15);

    let x_steady = transform(&steady, e_steady).translation.x;
    let x_hitched = transform(&hitched, e_hitched).translation.x;
    // Both runs ride the 2 m/s platform for the same 20 fixed steps: 0.8 m of +X travel.
    assert!(
        (x_steady - 0.8).abs() < 0.05,
        "steady pacing rides the platform: x {x_steady}"
    );
    assert!(
        (x_hitched - x_steady).abs() < 1e-3,
        "hitched pacing is carried once per fixed step like steady: steady {x_steady}, hitched {x_hitched}"
    );
    // The −Z root-motion distance is unchanged by the pacing.
    let z_steady = transform(&steady, e_steady).translation.z;
    let z_hitched = transform(&hitched, e_hitched).translation.z;
    assert!(
        (z_steady - z_hitched).abs() < 1e-3,
        "both pacings walk the same animation distance: steady {z_steady}, hitched {z_hitched}"
    );
}

/// fixrev-06-k H1 (dynamic-body variant): a crate the character runs into is pushed by
/// impulses solved once per `move_shape` call with the full fixed dt. A hitched bank drained
/// in several quantum calls in one fixed step pumped the full-dt impulse into the crate once
/// per quantum, with no physics integration between them. Draining one fixed step of root
/// motion per `step_character` makes the push sequence identical to steady pacing.
#[test]
fn hitched_root_motion_pushes_a_dynamic_body_like_steady_pacing() {
    const HZ: f64 = 50.0;
    let mk_app = || {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / HZ,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = HZ;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((CharacterPlugin, AnimationPlugin));
        use_rapier_character_controller(&mut app);
        static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
        app
    };
    // A heavy crate (64 kg: a 0.4 m cube at density 1000) standing in the walk line, close
    // enough that the hitched run's draining sweep (1 m in one fixed step) meets it mid-drain:
    // the quanta that follow must not pump repeated full-dt push impulses into it (10 m/s
    // covers 4 m in 0.4 s; contact at crate_z + 0.4 + 0.3 radius = -0.3, inside the drain's
    // five 0.2 m quanta). A light crate would tumble chaotically instead of sliding, which
    // measures tipping rather than the impulse total.
    let crate_at = |app: &mut App, z: f32| {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.4, z)),
                RigidBody::dynamic(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(0.4, 0.4, 0.4),
                    },
                    density: 1000.0,
                    ..Collider::default()
                },
            ))
            .id()
    };
    let walk = |app: &mut App| {
        spawn_with_clip(
            app,
            Vec3::new(0.0, REST_Y, 0.0),
            clip_speed(false, 10.0),
            RootMotionApply::Horizontal,
        )
    };

    let mut steady = mk_app();
    let crate_steady = crate_at(&mut steady, -1.0);
    let e_steady = walk(&mut steady);
    run(&mut steady, 20);

    let mut hitched = mk_app();
    let crate_hitched = crate_at(&mut hitched, -1.0);
    let e_hitched = walk(&mut hitched);
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(0.1)));
    hitched.update();
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / HZ,
    )));
    run(&mut hitched, 15);

    let crate_z_steady = transform(&steady, crate_steady).translation.z;
    let crate_z_hitched = transform(&hitched, crate_hitched).translation.z;
    assert!(
        crate_z_steady < -1.0,
        "steady pacing pushed the crate: z {crate_z_steady}"
    );
    // The push must stay in the per-step impulse regime (the impulse target is
    // remaining_translation / dt: a quantum's remaining is already its ~1/quanta share, so
    // the full dt per quantum reproduces the steady per-step push — measured residual 7 cm,
    // the compressed contact schedule of meeting the crate a few fixed steps earlier).
    // Scaling the impulse target by the sub-step instead (dt/quanta per quantum) targeted
    // quanta× the character speed and measured a 5.8 m overshoot here — the bound sits an
    // order of magnitude below that harmful regime and an order of magnitude above the
    // measured benign residual.
    assert!(
        (crate_z_steady - crate_z_hitched).abs() < 0.5,
        "both pacings push the crate within the contact-schedule residual: steady {crate_z_steady}, hitched {crate_z_hitched}"
    );
    let z_steady = transform(&steady, e_steady).translation.z;
    let z_hitched = transform(&hitched, e_hitched).translation.z;
    assert!(
        (z_steady - z_hitched).abs() < 1e-3,
        "both pacings stop at the crate together: steady {z_steady}, hitched {z_hitched}"
    );
}

/// fixrev-06-k M3: the old 32-quantum clamp reopened the tunneling/contact gap for banks over
/// 32·dt (the pending window reaches ~1 s). Draining at most one fixed step per
/// `step_character` has no clamp to reopen: a 0.7 s bank (35 fixed steps at 50 Hz) resolves
/// the wall exactly like steady pacing.
#[test]
fn large_root_motion_bank_over_32_fixed_steps_matches_steady_pacing() {
    const HZ: f64 = 50.0;
    let mk_app = || {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / HZ,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = HZ;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((CharacterPlugin, AnimationPlugin));
        use_rapier_character_controller(&mut app);
        // Ground and a thin wall whose face is at z = -0.5.
        static_box(&mut app, Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
        static_box(&mut app, Vec3::new(5.0, 2.0, 0.05), Vec3::new(0.0, 2.0, -0.55));
        app
    };
    // 10 m/s: a 0.7 s frame banks 7 m of root motion (35 fixed steps, over the old 32 cap).
    let walk = |app: &mut App| {
        spawn_with_clip(
            app,
            Vec3::new(0.0, REST_Y, 0.0),
            clip_speed(false, 10.0),
            RootMotionApply::Horizontal,
        )
    };

    // Steady: 45 frames of 1/50 s = 0.9 s of animation (9 m unblocked).
    let mut steady = mk_app();
    let e_steady = walk(&mut steady);
    run(&mut steady, 45);

    // Hitched: one 0.7 s frame (35 fixed steps' worth of wall time) then ten 1/50 s frames.
    let mut hitched = mk_app();
    let e_hitched = walk(&mut hitched);
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(0.7)));
    hitched.update();
    hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / HZ,
    )));
    run(&mut hitched, 10);

    let z_steady = transform(&steady, e_steady).translation.z;
    let z_hitched = transform(&hitched, e_hitched).translation.z;
    assert!(
        z_steady > -0.35,
        "steady pacing stops at the wall contact, got z {z_steady}"
    );
    assert!(
        z_hitched > -0.35,
        "the large-bank run did not tunnel through the wall: z {z_hitched}"
    );
    assert!(
        (z_steady - z_hitched).abs() < 1e-3,
        "both pacings resolve the wall identically: steady {z_steady}, hitched {z_hitched}"
    );
}
