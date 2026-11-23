// SPDX-License-Identifier: MPL-2.0
//! Physics robustness (specs/15 § 6 and ADR-870): stacking, CCD, joints, sleeping, removal
//! bookkeeping and events, driven through `PhysicsPlugin` the way a game uses it.
//!
//! Every test runs a fixed number of 60 Hz steps from deterministic inputs; there are no
//! wall-clock budgets. Tolerances are physical (centimeters, radians) and leave room for the
//! solver's soft contacts and joints, not for wrong behavior: each test also asserts that the
//! scenario actually happened (the swing reached its limit, the body really moved), so a
//! test cannot pass because nothing moved.
//!
//! Provenance: the expectations are elementary mechanics derived here (free fall, pendulum
//! energy, friction bounds); the simulation is rapier3d (Apache-2.0) behind Regia Engine's sync.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::message::{MessageCursor, Messages};
use regia_ecs::prelude::Entity;
use regia_physics::{
    ActiveEvents, BodyKind, Collider, ColliderHandle, ColliderShape, CollisionEnded, CollisionStarted,
    ExternalForce, ExternalImpulse, Joint, JointBroken, JointHandle, JointKind, JointMotorConfig,
    JointNoContacts, Mass, PhysicsPlugin, PhysicsPose, PhysicsSettings, PhysicsStats, PhysicsWorld,
    RigidBody, RigidBodyHandle, Teleport, TriggerEnter, TriggerExit, Velocity,
};
use regia_scene::Transform;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One fixed step per update: a manual 1/60 s frame clock at a 60 Hz fixed rate.
fn physics_app() -> App {
    app_with_frame_rate(60.0)
}

/// A 60 Hz fixed rate driven by a manual frame clock of `frame_hz` frames per second.
fn app_with_frame_rate(frame_hz: f64) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / frame_hz,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// A static slab whose top face is at y = `top`, `half` wide in X and Z, 1 m thick.
fn floor(app: &mut App, top: f32, half: f32) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, top - 0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(half, 0.5, half),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id()
}

/// A dynamic box with `half` extents at `at`, friction 0.8, no bounce.
fn dynamic_box(app: &mut App, half: Vec3, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                friction: 0.8,
                restitution: 0.0,
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id()
}

/// A static box (joint anchors).
fn static_box(app: &mut App, half: f32, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(half),
                },
                ..Collider::default()
            },
        ))
        .id()
}

fn steps(app: &mut App, count: usize) {
    for _ in 0..count {
        app.update();
    }
}

/// The body's pose after the latest fixed step (the `Transform` is the interpolated one).
fn pose(app: &App, entity: Entity) -> (Vec3, Quat) {
    if let Some(p) = app.world().get::<PhysicsPose>(entity) {
        return (p.translation, p.rotation);
    }
    let t = app.world().get::<Transform>(entity).expect("transform");
    (t.translation, t.rotation)
}

fn position(app: &App, entity: Entity) -> Vec3 {
    pose(app, entity).0
}

fn velocity(app: &App, entity: Entity) -> Velocity {
    app.world().get::<Velocity>(entity).cloned().unwrap_or_default()
}

fn physics(app: &App) -> &PhysicsWorld {
    app.world().resource::<PhysicsWorld>()
}

fn stats(app: &App) -> PhysicsStats {
    physics(app).stats()
}

/// Whether the rapier body of `entity` is asleep.
fn is_sleeping(app: &App, entity: Entity) -> bool {
    let handle = app
        .world()
        .get::<RigidBodyHandle>(entity)
        .expect("body handle")
        .to_handle();
    physics(app)
        .bodies
        .get(handle)
        .expect("rapier body")
        .is_sleeping()
}

/// Updates until a frame that ran at least one fixed step (bounded).
fn update_until_a_step_ran(app: &mut App) {
    let before = stats(app).steps;
    for _ in 0..16 {
        app.update();
        if stats(app).steps > before {
            return;
        }
    }
    panic!("no fixed step ran in 16 frames");
}

/// Messages of type `M` written since `cursor` last read. Messages live about two frames:
/// drain after every update (see [`steps_collecting`]).
fn drain<M: regia_ecs::ecs::message::Message + Clone>(app: &App, cursor: &mut MessageCursor<M>) -> Vec<M> {
    cursor
        .read(app.world().resource::<Messages<M>>())
        .cloned()
        .collect()
}

/// Runs `count` updates and returns every `M` message written meanwhile (drained each frame).
fn steps_collecting<M: regia_ecs::ecs::message::Message + Clone>(
    app: &mut App,
    cursor: &mut MessageCursor<M>,
    count: usize,
) -> Vec<M> {
    let mut out = Vec::new();
    for _ in 0..count {
        app.update();
        out.extend(drain(app, cursor));
    }
    out
}

/// World-space distance between a joint's two anchors (0 for a perfectly held joint).
fn anchor_gap(app: &App, joint_entity: Entity) -> f32 {
    let joint = app.world().get::<Joint>(joint_entity).expect("joint").clone();
    let (pa, ra) = pose(app, joint_entity);
    let (pb, rb) = pose(app, joint.connected);
    ((pa + ra * joint.anchor) - (pb + rb * joint.connected_anchor)).length()
}

// ---------------------------------------------------------------------------
// Spawn numerics (audit-v035-numerics M9 at the spawn seam)
// ---------------------------------------------------------------------------

/// A spawn `Transform.rotation` of zero is finite, so it used to pass `finite_pose` and reach
/// rapier verbatim — but rapier assumes unit rotations: a zero quaternion is a zero rotation
/// matrix (audit-v035-numerics M9). The spawn seam now runs `solver_rotation` like the
/// kinematic/teleport seams: the body parks at the identity rotation and the drop is counted
/// in `PhysicsStats::non_finite_inputs`.
#[test]
fn spawn_with_a_zero_rotation_parks_at_identity_and_counts() {
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(0.0, 3.0, 0.0),
            rotation: Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
            ..Transform::default()
        },
        RigidBody::dynamic(),
        Collider::default(),
    ));
    app.update();
    let world = app.world().resource::<PhysicsWorld>();
    let (_, rb) = world.bodies.iter().next().expect("the spawned body");
    let r = rb.position().rotation;
    let stored = [r.x.to_bits(), r.y.to_bits(), r.z.to_bits(), r.w.to_bits()];
    let identity = Quat::IDENTITY;
    assert_eq!(
        stored,
        [
            identity.x.to_bits(),
            identity.y.to_bits(),
            identity.z.to_bits(),
            identity.w.to_bits()
        ],
        "a zero spawn rotation parks at the identity rotation"
    );
    assert!(
        world.stats().non_finite_inputs >= 1,
        "the dropped rotation is counted"
    );
}

// ---------------------------------------------------------------------------
// Stacking
// ---------------------------------------------------------------------------

/// A 21-box pyramid (rows of 6..1) stands for 20 s: no box drifts 10 cm from where it was
/// placed, and the pile falls asleep.
#[test]
fn pyramid_of_21_boxes_holds_for_20_seconds() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    let half = 0.25f32;
    let pitch = 0.51f32; // 1 cm between neighbors
    let mut boxes = Vec::new();
    for row in 0..6 {
        let count = 6 - row;
        for i in 0..count {
            let x = (i as f32 - (count - 1) as f32 * 0.5) * pitch;
            let y = half + row as f32 * (2.0 * half + 0.001);
            let at = Vec3::new(x, y, 0.0);
            boxes.push((dynamic_box(&mut app, Vec3::splat(half), at), at));
        }
    }
    assert_eq!(boxes.len(), 21);

    steps(&mut app, 20 * 60);

    for (i, (entity, start)) in boxes.iter().enumerate() {
        let now = position(&app, *entity);
        assert!(
            (now - *start).length() < 0.1,
            "box {i} drifted: start {start:?}, now {now:?}"
        );
    }
    let s = stats(&app);
    assert_eq!(
        s.sleeping_dynamic, 21,
        "a settled pyramid sleeps (no solver cost): {s:?}"
    );
}

/// A 16-box tower (8 m) stands for 30 s with eight solver iterations, and the setting
/// inserted before the plugin is the one the plugin uses.
#[test]
fn sixteen_box_tower_holds_with_eight_solver_iterations() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    // Inserted before the plugin: the plugin must keep it (init, not overwrite).
    app.insert_resource(fixed).insert_resource(PhysicsSettings {
        solver_iterations: 8,
        ..PhysicsSettings::default()
    });
    app.add_plugins(PhysicsPlugin);
    assert_eq!(
        app.world().resource::<PhysicsSettings>().solver_iterations,
        8,
        "settings inserted before PhysicsPlugin are kept"
    );

    floor(&mut app, 0.0, 20.0);
    let half = 0.25f32;
    let tower: Vec<Entity> = (0..16)
        .map(|i| {
            let y = half + i as f32 * (2.0 * half + 0.001);
            dynamic_box(&mut app, Vec3::splat(half), Vec3::new(0.0, y, 0.0))
        })
        .collect();

    steps(&mut app, 30 * 60);

    for (i, &e) in tower.iter().enumerate() {
        let p = position(&app, e);
        let expected = half + i as f32 * 2.0 * half;
        assert!(
            p.x.abs() < 0.2 && p.z.abs() < 0.2 && (p.y - expected).abs() < 0.2,
            "box {i} of the tower moved: {p:?} (expected y≈{expected})"
        );
    }
}

/// A box 100× heavier resting on a light box: the light box is not crushed into the floor
/// and the heavy one does not sink into it.
#[test]
fn heavy_box_on_a_light_box_does_not_sink() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 10.0);
    let half = 0.25f32;
    // Density 1 → 0.125 kg; the heavy box adds 12.375 kg on top of its own 0.125 kg.
    let light = dynamic_box(&mut app, Vec3::splat(half), Vec3::new(0.0, half, 0.0));
    let heavy = dynamic_box(
        &mut app,
        Vec3::splat(half),
        Vec3::new(0.0, 3.0 * half + 0.001, 0.0),
    );
    app.world_mut().entity_mut(heavy).insert(Mass::Explicit {
        mass: 12.375,
        center_of_mass: Vec3::ZERO,
        principal_inertia: Vec3::splat(12.375 * (2.0 * half) * (2.0 * half) / 6.0),
    });

    steps(&mut app, 10 * 60);

    let pl = position(&app, light);
    let ph = position(&app, heavy);
    assert!(
        pl.y > half - 0.08,
        "the light box was pushed into the floor: {pl:?}"
    );
    assert!(
        ph.y - pl.y > 2.0 * half - 0.1,
        "the heavy box sank into the light one: heavy {ph:?}, light {pl:?}"
    );
    assert!(
        (ph.x - pl.x).abs() < 0.1 && (ph.z - pl.z).abs() < 0.1,
        "the pair stayed stacked: heavy {ph:?}, light {pl:?}"
    );
}

// ---------------------------------------------------------------------------
// Continuous collision detection
// ---------------------------------------------------------------------------

/// A CCD ball fired at 400 m/s (6.7 m per step) at a 5 cm wall stops at the wall.
#[test]
fn ccd_ball_does_not_tunnel_through_a_thin_wall() {
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.025, 4.0, 4.0),
            },
            ..Collider::default()
        },
    ));
    let ball = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody {
                ccd: true,
                ..RigidBody::dynamic()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(400.0, 0.0, 0.0),
                angular: Vec3::ZERO,
            },
        ))
        .id();

    steps(&mut app, 30);

    assert_eq!(stats(&app).ccd_bodies, 1, "CCD reached rapier");
    let x = position(&app, ball).x;
    assert!(x > 3.0, "the ball travelled to the wall: x={x}");
    assert!(x < 5.0, "the CCD ball did not tunnel through the wall: x={x}");
}

/// CCD switched on at runtime (a `RigidBody` edit) takes effect before the shot.
#[test]
fn ccd_enabled_at_runtime_takes_effect() {
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.025, 4.0, 4.0),
            },
            ..Collider::default()
        },
    ));
    let ball = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody {
                gravity_scale: 0.0,
                ..RigidBody::dynamic()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                ..Collider::default()
            },
            Velocity::default(),
        ))
        .id();
    steps(&mut app, 2);
    assert_eq!(stats(&app).ccd_bodies, 0);

    {
        let mut world = app.world_mut().entity_mut(ball);
        world.get_mut::<RigidBody>().expect("body").ccd = true;
        world.get_mut::<Velocity>().expect("velocity").linear = Vec3::new(400.0, 0.0, 0.0);
    }
    steps(&mut app, 30);

    assert_eq!(stats(&app).ccd_bodies, 1, "the CCD edit reached rapier");
    let x = position(&app, ball).x;
    assert!(x > 3.0 && x < 5.0, "stopped at the wall: x={x}");
}

/// A CCD ball fired at 300 m/s (5 m per step) head-on into a ball rolling towards it hits the
/// ball instead of passing through it. rapier 0.36 sweeps a CCD ("bullet") body against the
/// other bodies at their end-of-step pose, so the target here is slow: two fast bodies closing
/// head-on can pass through each other (ADR-1387, `book/agents/physics.md`).
#[test]
fn a_ccd_ball_fired_head_on_into_a_rolling_ball_does_not_pass_through_it() {
    let mut app = physics_app();
    let spawn = |app: &mut App, x: f32, vx: f32, ccd: bool| {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                RigidBody {
                    ccd,
                    gravity_scale: 0.0,
                    ..RigidBody::dynamic()
                },
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.1 },
                    ..Collider::default()
                },
                Velocity {
                    linear: Vec3::new(vx, 0.0, 0.0),
                    angular: Vec3::ZERO,
                },
            ))
            .id()
    };
    let left = spawn(&mut app, -12.0, 300.0, true);
    let right = spawn(&mut app, 12.0, -2.0, false);

    steps(&mut app, 10);

    let (xl, xr) = (position(&app, left).x, position(&app, right).x);
    assert!(
        xl < xr,
        "the balls passed through each other: left {xl}, right {xr}"
    );
}

// ---------------------------------------------------------------------------
// Joints
// ---------------------------------------------------------------------------

/// A 10-link ball-joint chain, kicked sideways, stays connected: no anchor pair separates
/// by more than 5 cm at any step of 10 s.
#[test]
fn spherical_chain_of_ten_links_stays_connected() {
    let mut app = physics_app();
    let anchor = static_box(&mut app, 0.1, Vec3::new(0.0, 10.0, 0.0));
    let mut parent = anchor;
    let mut joints = Vec::new();
    let mut last = anchor;
    for i in 0..10 {
        let link = dynamic_box(
            &mut app,
            Vec3::splat(0.1),
            Vec3::new(0.0, 9.5 - 0.5 * i as f32, 0.0),
        );
        app.world_mut().entity_mut(parent).insert(Joint {
            kind: JointKind::Spherical { cone_limit: None },
            connected: link,
            anchor: Vec3::new(0.0, -0.25, 0.0),
            connected_anchor: Vec3::new(0.0, 0.25, 0.0),
            break_force: 0.0,
        });
        joints.push(parent);
        parent = link;
        last = link;
    }
    app.update();
    app.world_mut()
        .get_mut::<Velocity>(last)
        .expect("velocity")
        .linear = Vec3::new(3.0, 0.0, 1.0);

    let mut worst = 0.0f32;
    for _ in 0..600 {
        app.update();
        for &j in &joints {
            worst = worst.max(anchor_gap(&app, j));
        }
    }
    assert_eq!(physics(&app).impulse_joints.len(), 10);
    assert!(worst < 0.1, "a joint opened by {worst} m");
    let tail = position(&app, last);
    assert!(tail.y < 9.0, "the chain hangs: tail {tail:?}");
}

/// A revolute arm with ±0.5 rad limits, swung hard enough to reach 0.8 rad unconstrained,
/// stops at its limits.
#[test]
fn revolute_limits_hold_a_swinging_arm() {
    let mut app = physics_app();
    let anchor = static_box(&mut app, 0.1, Vec3::new(0.0, 5.0, 0.0));
    // A 1 m arm hanging from the pivot at y = 4.9 (center 0.5 m below it).
    let arm = dynamic_box(&mut app, Vec3::new(0.05, 0.5, 0.05), Vec3::new(0.0, 4.4, 0.0));
    app.world_mut().entity_mut(anchor).insert((
        Joint {
            kind: JointKind::Revolute {
                axis: Vec3::Z,
                limits: Some((-0.5, 0.5)),
                motor: None,
            },
            connected: arm,
            anchor: Vec3::new(0.0, -0.1, 0.0),
            connected_anchor: Vec3::new(0.0, 0.5, 0.0),
            break_force: 0.0,
        },
        JointNoContacts,
    ));
    app.update();
    // 3 rad/s about the pivot: ½(mL²/3)ω² = mg(L/2)(1 − cos θ) gives θ ≈ 0.8 rad unlimited.
    // The center sits 0.5 m below the pivot, so it moves at ω × r = (1.5, 0, 0) m/s.
    *app.world_mut().get_mut::<Velocity>(arm).expect("velocity") = Velocity {
        linear: Vec3::new(1.5, 0.0, 0.0),
        angular: Vec3::new(0.0, 0.0, 3.0),
    };

    let mut widest = 0.0f32;
    for _ in 0..300 {
        app.update();
        let (_, rotation) = pose(&app, arm);
        let down = rotation * Vec3::NEG_Y;
        let angle = down.x.atan2(-down.y).abs();
        widest = widest.max(angle);
    }
    assert!(widest > 0.4, "the swing reached the limit: widest {widest} rad");
    assert!(widest < 0.62, "the limit held: widest {widest} rad");
    assert!(anchor_gap(&app, anchor) < 0.05, "the hinge stayed attached");
}

/// A prismatic slider with ±1 m limits, launched at 6 m/s, stops at its limit and never
/// leaves its axis.
#[test]
fn prismatic_limits_hold_a_launched_slider() {
    let mut app = physics_app();
    let anchor = static_box(&mut app, 0.2, Vec3::new(0.0, 2.0, 0.0));
    let slider = dynamic_box(&mut app, Vec3::splat(0.2), Vec3::new(0.0, 2.0, 0.0));
    app.world_mut().entity_mut(anchor).insert((
        Joint {
            kind: JointKind::Prismatic {
                axis: Vec3::X,
                limits: Some((-1.0, 1.0)),
                motor: None,
            },
            connected: slider,
            ..Joint::default()
        },
        JointNoContacts,
    ));
    app.update();
    app.world_mut()
        .get_mut::<Velocity>(slider)
        .expect("velocity")
        .linear = Vec3::new(6.0, 0.0, 0.0);

    let mut farthest = 0.0f32;
    for _ in 0..240 {
        app.update();
        let p = position(&app, slider);
        farthest = farthest.max(p.x.abs());
        assert!(
            (p.y - 2.0).abs() < 0.05 && p.z.abs() < 0.05,
            "the slider left its axis: {p:?}"
        );
    }
    assert!(farthest > 0.9, "the slider reached its limit: {farthest}");
    assert!(farthest < 1.2, "the limit held: {farthest}");
}

/// A revolute motor with a velocity target spins a free wheel at that rate.
#[test]
fn revolute_motor_reaches_its_target_velocity() {
    let mut app = physics_app();
    let anchor = static_box(&mut app, 0.1, Vec3::new(0.0, 2.0, 0.0));
    let wheel = dynamic_box(&mut app, Vec3::new(0.5, 0.05, 0.5), Vec3::new(0.0, 2.0, 0.0));
    app.world_mut().entity_mut(anchor).insert((
        Joint {
            kind: JointKind::Revolute {
                axis: Vec3::Y,
                limits: None,
                motor: Some(JointMotorConfig {
                    target_position: 0.0,
                    target_velocity: 2.0,
                    stiffness: 0.0,
                    damping: 50.0,
                    max_force: 0.0,
                }),
            },
            connected: wheel,
            ..Joint::default()
        },
        JointNoContacts,
    ));

    steps(&mut app, 180);

    let spin = velocity(&app, wheel).angular;
    assert!(
        (spin.y.abs() - 2.0).abs() < 0.4,
        "the motor drives the wheel at 2 rad/s about Y: {spin:?}"
    );
    assert!(
        spin.x.abs() < 0.1 && spin.z.abs() < 0.1,
        "only about the hinge: {spin:?}"
    );
}

/// The rebuilt joint after a `Joint` edit keeps a valid handle, so `break_force` set by the
/// edit still breaks it (the handle used to lose its generation on rebuild); a broken joint
/// stays broken until the next edit, which brings it back.
#[test]
fn edited_joint_still_breaks_and_stays_broken_until_edited_again() {
    let mut app = physics_app();
    // The plank hangs 0.5 m under the anchor's joint point (no overlap with the anchor).
    let anchor = static_box(&mut app, 0.2, Vec3::new(0.0, 4.0, 0.0));
    let plank = dynamic_box(&mut app, Vec3::splat(0.5), Vec3::new(0.0, 3.0, 0.0));
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Fixed,
        connected: plank,
        anchor: Vec3::new(0.0, -0.5, 0.0),
        connected_anchor: Vec3::new(0.0, 0.5, 0.0),
        break_force: 0.0,
    });
    steps(&mut app, 5);
    // The edit rebuilds the joint in a recycled rapier slot (new generation).
    app.world_mut()
        .get_mut::<Joint>(anchor)
        .expect("joint")
        .break_force = 80.0;
    steps(&mut app, 1);
    let handle = *app
        .world()
        .get::<JointHandle>(anchor)
        .expect("rebuilt joint handle");
    assert!(
        physics(&app).impulse_joints.get(handle.to_handle()).is_some(),
        "the JointHandle component names the live rapier joint: {handle:?}"
    );

    // Load the plank with 100 kg: ≈ 990 N hanging from an 80 N joint. (A dropped weight
    // would land on the static anchor box above the plank, not on the plank.)
    app.world_mut().entity_mut(plank).insert(Mass::Explicit {
        mass: 100.0,
        center_of_mass: Vec3::ZERO,
        principal_inertia: Vec3::splat(10.0),
    });
    let mut cursor = MessageCursor::<JointBroken>::default();
    let mut broken = Vec::new();
    for _ in 0..180 {
        app.update();
        broken.extend(drain(&app, &mut cursor));
    }
    assert_eq!(broken.len(), 1, "the edited joint broke once: {broken:?}");
    assert_eq!(broken[0].entity, anchor);
    assert_eq!(broken[0].connected, plank);

    // No resurrection while nothing changes.
    steps(&mut app, 60);
    assert_eq!(
        physics(&app).impulse_joints.len(),
        0,
        "a broken joint stays broken"
    );
    assert!(app.world().get::<JointHandle>(anchor).is_none());

    // Editing the joint lifts the break.
    app.world_mut()
        .get_mut::<Joint>(anchor)
        .expect("joint")
        .break_force = 0.0;
    steps(&mut app, 1);
    assert_eq!(
        physics(&app).impulse_joints.len(),
        1,
        "the edit re-creates the joint"
    );
}

/// A joint whose connected body is removed comes back when the body returns, and the
/// returning body gets its collider back.
#[test]
fn joint_and_collider_return_with_a_re_added_body() {
    let mut app = physics_app();
    let anchor = static_box(&mut app, 0.1, Vec3::new(0.0, 5.0, 0.0));
    let link = dynamic_box(&mut app, Vec3::splat(0.1), Vec3::new(0.0, 4.5, 0.0));
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Spherical { cone_limit: None },
        connected: link,
        anchor: Vec3::new(0.0, -0.25, 0.0),
        connected_anchor: Vec3::new(0.0, 0.25, 0.0),
        break_force: 0.0,
    });
    steps(&mut app, 5);
    assert_eq!(physics(&app).impulse_joints.len(), 1);
    assert_eq!(physics(&app).colliders.len(), 2);

    app.world_mut().entity_mut(link).remove::<RigidBody>();
    steps(&mut app, 2);
    assert_eq!(
        physics(&app).impulse_joints.len(),
        0,
        "the joint went with the body"
    );
    assert_eq!(physics(&app).bodies.len(), 1, "only the anchor is left");
    assert!(
        app.world().get::<JointHandle>(anchor).is_none(),
        "no stale joint handle"
    );
    assert!(
        app.world().get::<ColliderHandle>(link).is_none(),
        "no stale collider handle"
    );

    app.world_mut().entity_mut(link).insert(RigidBody::dynamic());
    steps(&mut app, 2);
    assert_eq!(physics(&app).impulse_joints.len(), 1, "the joint came back");
    assert_eq!(physics(&app).colliders.len(), 2, "the link's collider came back");

    steps(&mut app, 60);
    assert!(anchor_gap(&app, anchor) < 0.05, "the returned joint holds");
}

/// A joint whose connected entity gets its body later is created on that step.
#[test]
fn joint_waits_for_a_connected_body_added_later() {
    let mut app = physics_app();
    let anchor = static_box(&mut app, 0.1, Vec3::new(0.0, 5.0, 0.0));
    let later = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(0.0, 4.5, 0.0)))
        .id();
    app.world_mut().entity_mut(anchor).insert(Joint {
        kind: JointKind::Spherical { cone_limit: None },
        connected: later,
        anchor: Vec3::new(0.0, -0.25, 0.0),
        connected_anchor: Vec3::new(0.0, 0.25, 0.0),
        break_force: 0.0,
    });
    steps(&mut app, 3);
    assert_eq!(physics(&app).impulse_joints.len(), 0, "nothing to connect yet");

    app.world_mut().entity_mut(later).insert((
        RigidBody::dynamic(),
        Collider::default(),
        Velocity::default(),
    ));
    steps(&mut app, 1);
    assert_eq!(
        physics(&app).impulse_joints.len(),
        1,
        "created once the body exists"
    );
    assert!(app.world().get::<JointHandle>(anchor).is_some());
}

/// A joint connecting a body to itself is skipped instead of reaching rapier.
#[test]
fn joint_to_its_own_body_is_skipped() {
    let mut app = physics_app();
    let e = dynamic_box(&mut app, Vec3::splat(0.2), Vec3::new(0.0, 2.0, 0.0));
    app.world_mut().entity_mut(e).insert(Joint {
        connected: e,
        ..Joint::default()
    });
    steps(&mut app, 3);
    assert_eq!(physics(&app).impulse_joints.len(), 0);
    assert!(app.world().get::<JointHandle>(e).is_none());
}

// ---------------------------------------------------------------------------
// Sleeping
// ---------------------------------------------------------------------------

/// Boxes at rest fall asleep, and asleep they are frozen bit-exactly.
#[test]
fn resting_boxes_fall_asleep_and_stay_frozen() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    for i in 0..10 {
        dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(i as f32 - 4.5, 0.5, 0.0));
    }
    steps(&mut app, 5 * 60);
    let s = stats(&app);
    assert_eq!((s.sleeping_dynamic, s.awake_dynamic), (10, 0), "{s:?}");

    let digest = physics(&app).state_digest();
    steps(&mut app, 60);
    assert_eq!(
        physics(&app).state_digest(),
        digest,
        "sleeping bodies do not drift"
    );
}

/// The 10-box tower of specs/15 § 6 falls asleep as one island.
#[test]
fn ten_box_tower_falls_asleep() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    let half = 0.25f32;
    for i in 0..10 {
        let y = half + i as f32 * (2.0 * half + 0.001);
        dynamic_box(&mut app, Vec3::splat(half), Vec3::new(0.0, y, 0.0));
    }
    steps(&mut app, 10 * 60);
    let s = stats(&app);
    assert_eq!(s.sleeping_dynamic, 10, "{s:?}");
}

/// Sleeping boxes wake and fall when the floor under them is despawned, or loses its
/// `Collider` component.
#[test]
fn sleeping_boxes_fall_when_their_floor_goes_away() {
    for remove_component_only in [false, true] {
        let mut app = physics_app();
        floor(&mut app, -5.0, 20.0); // the catch floor
        let shelf = floor(&mut app, 0.0, 2.0);
        let boxes: Vec<Entity> = (0..3)
            .map(|i| dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(i as f32 - 1.0, 0.25, 0.0)))
            .collect();
        steps(&mut app, 5 * 60);
        assert!(boxes.iter().all(|&b| is_sleeping(&app, b)), "asleep on the shelf");

        if remove_component_only {
            app.world_mut().entity_mut(shelf).remove::<Collider>();
        } else {
            app.world_mut().entity_mut(shelf).despawn();
        }
        steps(&mut app, 60);
        for &b in &boxes {
            let y = position(&app, b).y;
            assert!(
                y < -2.0,
                "a box left floating when the shelf went away (component only: \
                 {remove_component_only}): y={y}"
            );
        }
    }
}

/// An impulse and a velocity write each wake a sleeping body.
#[test]
fn impulse_and_velocity_writes_wake_sleeping_bodies() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    let kicked = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(-2.0, 0.25, 0.0));
    let thrown = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(2.0, 0.25, 0.0));
    steps(&mut app, 5 * 60);
    assert!(is_sleeping(&app, kicked) && is_sleeping(&app, thrown));

    // 0.125 kg box: 0.5 N·s up is 4 m/s.
    app.world_mut().entity_mut(kicked).insert(ExternalImpulse {
        impulse: Vec3::new(0.0, 0.5, 0.0),
        torque_impulse: Vec3::ZERO,
    });
    app.world_mut()
        .get_mut::<Velocity>(thrown)
        .expect("velocity")
        .linear = Vec3::new(0.0, 4.0, 0.0);
    steps(&mut app, 12);
    for (name, e) in [("impulse", kicked), ("velocity write", thrown)] {
        let y = position(&app, e).y;
        assert!(y > 0.6, "the {name} woke the body and lifted it: y={y}");
    }
}

/// `can_sleep: false` keeps a resting body awake.
#[test]
fn can_sleep_false_never_sleeps() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    let e = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 0.25, 0.0));
    app.world_mut().get_mut::<RigidBody>(e).expect("body").can_sleep = false;
    steps(&mut app, 5 * 60);
    assert!(!is_sleeping(&app, e));
    assert_eq!(stats(&app).awake_dynamic, 1);
}

/// A constant `ExternalForce` pressing a box into the floor lets it sleep (the force used to
/// be re-pushed, waking the body, every step); editing the force wakes it and moves it.
#[test]
fn constant_force_lets_a_resting_body_sleep_and_an_edit_wakes_it() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 50.0);
    let e = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 0.25, 0.0));
    app.world_mut().entity_mut(e).insert(ExternalForce {
        force: Vec3::new(0.0, -5.0, 0.0),
        torque: Vec3::ZERO,
    });
    steps(&mut app, 5 * 60);
    assert!(
        is_sleeping(&app, e),
        "a body held still by a constant force sleeps"
    );

    // 10 N sideways against ≈ 0.8 × (1.2 + 5) N of friction: ≈ 40 m/s² of slide.
    app.world_mut().get_mut::<ExternalForce>(e).expect("force").force = Vec3::new(10.0, -5.0, 0.0);
    steps(&mut app, 60);
    let x = position(&app, e).x;
    assert!(x > 0.5, "the edited force woke the body and pushed it: x={x}");
}

/// Removing `ExternalForce` stops the force (rapier keeps user forces until reset).
#[test]
fn removing_external_force_stops_it() {
    let mut app = physics_app();
    let e = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 0.0, 0.0));
    // 3 N up on 0.125 kg (1.23 N of weight): ≈ 14 m/s² upward.
    app.world_mut().entity_mut(e).insert(ExternalForce {
        force: Vec3::new(0.0, 3.0, 0.0),
        torque: Vec3::ZERO,
    });
    steps(&mut app, 60);
    let rising = velocity(&app, e).linear.y;
    assert!(rising > 8.0, "the force lifts the box: vy={rising}");

    app.world_mut().entity_mut(e).remove::<ExternalForce>();
    steps(&mut app, 3 * 60);
    let vy = velocity(&app, e).linear.y;
    // Free fall from ≈14 m/s for 3 s ends near −15 m/s; a lingering force keeps climbing.
    assert!(vy < -5.0, "the removed force stopped pushing: vy={vy}");
}

// ---------------------------------------------------------------------------
// Removal bookkeeping and events
// ---------------------------------------------------------------------------

/// At 240 frames per second with 60 Hz physics, three frames pass between fixed steps. A
/// despawn right after a step must still remove the rapier body (removal messages are
/// cleared every frame; the plugin records removals with observers instead).
#[test]
fn despawn_between_fixed_steps_leaves_no_ghost_body() {
    let mut app = app_with_frame_rate(240.0);
    floor(&mut app, 0.0, 20.0);
    let doomed = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 0.25, 0.0));
    let survivor = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(3.0, 0.25, 0.0));
    for _ in 0..3 {
        update_until_a_step_ran(&mut app);
    }
    assert_eq!(physics(&app).bodies.len(), 3);

    app.world_mut().despawn(doomed);
    let before = stats(&app).steps;
    let mut frames = 0;
    while stats(&app).steps < before + 2 {
        app.update();
        frames += 1;
        assert!(frames < 64, "fixed steps stopped running");
    }
    assert!(frames >= 4, "frames ran without a fixed step: {frames}");
    let world = physics(&app);
    assert_eq!(world.bodies.len(), 2, "the despawned body left the rapier world");
    assert_eq!(world.colliders.len(), 2);
    assert!(
        world
            .entity_of_body(
                app.world()
                    .get::<RigidBodyHandle>(survivor)
                    .expect("h")
                    .to_handle()
            )
            .is_some()
    );
}

/// A body removed and re-added in the same frame is rebuilt, not lost: the stale removal
/// record must not take down the new body.
#[test]
fn body_removed_and_re_added_in_one_frame_is_rebuilt() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    let e = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 2.0, 0.0));
    steps(&mut app, 3);
    {
        let mut entity = app.world_mut().entity_mut(e);
        entity.remove::<RigidBody>();
        entity.insert(RigidBody::dynamic());
    }
    steps(&mut app, 3);
    let world = physics(&app);
    assert_eq!(world.bodies.len(), 2, "floor + the rebuilt body");
    assert_eq!(world.colliders.len(), 2, "the collider was rebuilt with it");
    let handle = app.world().get::<RigidBodyHandle>(e).expect("handle").to_handle();
    assert_eq!(world.entity_of_body(handle), Some(e));
    steps(&mut app, 120);
    let y = position(&app, e).y;
    assert!(
        (y - 0.25).abs() < 0.05,
        "the rebuilt body landed on the floor: y={y}"
    );
}

/// Despawning a body that touches another reports `CollisionEnded` for the pair.
#[test]
fn despawning_a_touching_body_reports_collision_ended() {
    let mut app = physics_app();
    let ground = floor(&mut app, 0.0, 20.0);
    app.world_mut()
        .get_mut::<Collider>(ground)
        .expect("collider")
        .active_events = ActiveEvents::ALL;
    let crate_box = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 0.5, 0.0));
    let mut started = MessageCursor::<CollisionStarted>::default();
    let mut ended = MessageCursor::<CollisionEnded>::default();
    let mut landed = 0;
    for _ in 0..60 {
        app.update();
        landed += drain(&app, &mut started).len();
        // Forget any end raised by a settling bounce: only the despawn's end counts below.
        drain(&app, &mut ended);
    }
    assert!(landed >= 1, "the box landed");

    app.world_mut().despawn(crate_box);
    let ends = steps_collecting(&mut app, &mut ended, 2);
    assert_eq!(ends.len(), 1, "exactly one end for the despawned pair: {ends:?}");
    let pair = [ends[0].entity_a, ends[0].entity_b];
    assert!(pair.contains(&ground) && pair.contains(&crate_box), "{pair:?}");
}

/// Despawning a body inside a sensor reports `TriggerExit`; so does despawning the sensor.
#[test]
fn despawning_inside_or_as_a_sensor_reports_trigger_exit() {
    for despawn_sensor in [false, true] {
        let mut app = physics_app();
        let sensor = app
            .world_mut()
            .spawn((
                Transform::IDENTITY,
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(2.0),
                    },
                    sensor: true,
                    active_events: ActiveEvents::ALL,
                    ..Collider::default()
                },
            ))
            .id();
        let visitor = app
            .world_mut()
            .spawn((
                Transform::IDENTITY,
                RigidBody {
                    gravity_scale: 0.0,
                    ..RigidBody::dynamic()
                },
                Collider::default(),
            ))
            .id();
        let mut enter = MessageCursor::<TriggerEnter>::default();
        let mut exit = MessageCursor::<TriggerExit>::default();
        let entered = steps_collecting(&mut app, &mut enter, 5);
        assert_eq!(entered.len(), 1, "the visitor is inside: {entered:?}");

        app.world_mut()
            .despawn(if despawn_sensor { sensor } else { visitor });
        let exits = steps_collecting(&mut app, &mut exit, 2);
        assert_eq!(
            exits,
            vec![TriggerExit {
                trigger: sensor,
                other: visitor
            }],
            "one exit when the {} is despawned",
            if despawn_sensor { "sensor" } else { "visitor" }
        );
    }
}

/// A body switched from Dynamic to Kinematic hands its `Transform` back to gameplay: its old
/// interpolation poses no longer overwrite the `Transform` every frame.
#[test]
fn switching_to_kinematic_hands_the_transform_to_gameplay() {
    let mut app = physics_app();
    let e = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 5.0, 0.0));
    steps(&mut app, 30);
    app.world_mut().get_mut::<RigidBody>(e).expect("body").kind = BodyKind::Kinematic;
    steps(&mut app, 1);
    assert!(
        app.world().get::<PhysicsPose>(e).is_none(),
        "poses dropped with the kind"
    );

    let start = app.world().get::<Transform>(e).expect("transform").translation;
    for _ in 0..60 {
        app.world_mut()
            .get_mut::<Transform>(e)
            .expect("transform")
            .translation
            .x += 1.0 / 60.0;
        app.update();
    }
    let end = app.world().get::<Transform>(e).expect("transform").translation;
    assert!(
        (end.x - start.x - 1.0).abs() < 0.01,
        "gameplay moved the kinematic body 1 m: {start:?} → {end:?}"
    );
    assert!(
        (end.y - start.y).abs() < 1e-4,
        "no gravity on a kinematic body: {start:?} → {end:?}"
    );
    let handle = app.world().get::<RigidBodyHandle>(e).expect("handle").to_handle();
    let rapier_x = physics(&app)
        .bodies
        .get(handle)
        .expect("body")
        .position()
        .translation
        .x;
    assert!(
        (rapier_x - end.x).abs() < 0.01,
        "rapier follows: {rapier_x} vs {}",
        end.x
    );
}

/// A teleported kinematic body keeps following its `Transform` afterwards (a teleport used to
/// pin interpolation poses on it that nothing updated again).
#[test]
fn teleported_kinematic_body_keeps_following_its_transform() {
    let mut app = physics_app();
    let e = app
        .world_mut()
        .spawn((Transform::IDENTITY, RigidBody::kinematic(), Collider::default()))
        .id();
    steps(&mut app, 3);
    app.world_mut()
        .get_mut::<Transform>(e)
        .expect("transform")
        .translation = Vec3::new(10.0, 0.0, 0.0);
    app.world_mut().entity_mut(e).insert(Teleport);
    steps(&mut app, 2);
    assert!(app.world().get::<Teleport>(e).is_none(), "the marker is consumed");
    assert!(
        app.world().get::<PhysicsPose>(e).is_none(),
        "no poses on a kinematic body"
    );
    for _ in 0..30 {
        app.world_mut()
            .get_mut::<Transform>(e)
            .expect("transform")
            .translation
            .y += 0.1;
        app.update();
    }
    let t = app.world().get::<Transform>(e).expect("transform").translation;
    assert!(
        (t.x - 10.0).abs() < 1e-4 && (t.y - 3.0).abs() < 0.01,
        "follows gameplay: {t:?}"
    );
}

/// Removing `RigidBody` (a prop picked up and parented to a hand) leaves the `Transform` to
/// gameplay: the interpolation stops writing it.
#[test]
fn removing_rigid_body_leaves_the_transform_to_gameplay() {
    let mut app = physics_app();
    let e = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 5.0, 0.0));
    steps(&mut app, 10);
    app.world_mut().entity_mut(e).remove::<RigidBody>();
    steps(&mut app, 1);
    let target = Vec3::new(7.0, 7.0, 7.0);
    app.world_mut()
        .get_mut::<Transform>(e)
        .expect("transform")
        .translation = target;
    steps(&mut app, 3);
    assert_eq!(
        app.world().get::<Transform>(e).expect("transform").translation,
        target
    );
    assert!(app.world().get::<PhysicsPose>(e).is_none());
    assert_eq!(physics(&app).bodies.len(), 0);
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// `PhysicsWorld::stats` counts what was spawned.
#[test]
fn stats_count_bodies_colliders_and_joints() {
    let mut app = physics_app();
    floor(&mut app, 0.0, 20.0);
    let a = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 3.0, 0.0));
    let b = dynamic_box(&mut app, Vec3::splat(0.25), Vec3::new(0.0, 2.0, 0.0));
    app.world_mut().get_mut::<RigidBody>(b).expect("body").ccd = true;
    app.world_mut().entity_mut(a).insert(Joint {
        kind: JointKind::Spherical { cone_limit: None },
        connected: b,
        anchor: Vec3::new(0.0, -0.5, 0.0),
        connected_anchor: Vec3::new(0.0, 0.5, 0.0),
        break_force: 0.0,
    });
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(5.0, 1.0, 0.0)),
        RigidBody::kinematic(),
        Collider {
            sensor: true,
            ..Collider::default()
        },
    ));
    steps(&mut app, 2);
    let s = stats(&app);
    assert_eq!(s.bodies, 4, "{s:?}");
    assert_eq!((s.dynamic, s.kinematic, s.fixed), (2, 1, 1), "{s:?}");
    assert_eq!(s.colliders, 4, "{s:?}");
    assert_eq!(s.sensors, 1, "{s:?}");
    assert_eq!(s.joints, 1, "{s:?}");
    assert_eq!(s.ccd_bodies, 1, "{s:?}");
    assert_eq!(s.awake_dynamic + s.sleeping_dynamic, 2, "{s:?}");
    assert!(s.steps >= 1, "{s:?}");
}
