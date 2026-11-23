// SPDX-License-Identifier: MPL-2.0
//! Property tests over generated physics scenes (ADR-870).
//!
//! A seeded xorshift generator builds many small random scenes (shapes, sizes, rotations,
//! heights, velocities, chains); every scene must satisfy the same physical properties:
//!
//! - no pose or velocity ever becomes NaN or infinite;
//! - nothing ends up inside or under the floor (no tunneling, no unresolved penetration);
//! - a passive scene never gains mechanical energy (kinetic + gravitational);
//! - the same scene simulated twice is bit-identical;
//! - generated ball-joint chains stay connected.
//!
//! The seed is fixed, so a failure reproduces exactly; the failing case index and its
//! inputs are in the panic message. No new dependency: the generator is the same pattern
//! as `regia_platform`'s path property tests.
//!
//! Provenance: energy bookkeeping is elementary mechanics (½mv² + ½ωᵀIω + mgh, through
//! rapier's `kinetic_energy` / `gravitational_potential_energy`, Apache-2.0); the scene
//! generator is Regia Engine's own.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{EulerRot, Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, Joint, JointKind, JointNoContacts, PhysicsPlugin, PhysicsPose, PhysicsWorld,
    RigidBody, RigidBodyHandle, Velocity,
};
use regia_scene::Transform;

/// Tiny deterministic generator (xorshift64).
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in `[lo, hi)`.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        lo + (hi - lo) * unit
    }

    /// Uniform integer in `[lo, hi]`.
    fn int(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }

    fn vec(&mut self, magnitude: f32) -> Vec3 {
        Vec3::new(
            self.range(-magnitude, magnitude),
            self.range(-magnitude, magnitude),
            self.range(-magnitude, magnitude),
        )
    }
}

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

/// One generated dynamic body.
#[derive(Debug, Clone)]
struct BodySpec {
    shape: ColliderShape,
    /// Distance from the center to the nearest face (box) or the radius (ball): the
    /// lowest a resting center can be above the floor.
    min_extent: f32,
    at: Vec3,
    rotation: Quat,
    linear: Vec3,
    angular: Vec3,
    friction: f32,
}

fn random_body(rng: &mut XorShift, at: Vec3) -> BodySpec {
    let (shape, min_extent) = if rng.next().is_multiple_of(2) {
        let r = rng.range(0.1, 0.5);
        (ColliderShape::Sphere { radius: r }, r)
    } else {
        let h = Vec3::new(rng.range(0.1, 0.5), rng.range(0.1, 0.5), rng.range(0.1, 0.5));
        (ColliderShape::Box { half_extents: h }, h.min_element())
    };
    BodySpec {
        shape,
        min_extent,
        at,
        rotation: Quat::from_euler(
            EulerRot::XYZ,
            rng.range(-3.1, 3.1),
            rng.range(-3.1, 3.1),
            rng.range(-3.1, 3.1),
        ),
        linear: rng.vec(10.0) / 3f32.sqrt(),
        angular: rng.vec(5.0),
        friction: rng.range(0.0, 1.2),
    }
}

/// 1–10 bodies on a 2 m grid (no initial overlap: the largest extent is √3 × 0.5 m),
/// 1–6 m above the floor.
fn random_scene(rng: &mut XorShift) -> Vec<BodySpec> {
    let count = rng.int(1, 10);
    (0..count)
        .map(|i| {
            let at = Vec3::new(
                (i % 4) as f32 * 2.0 - 3.0,
                rng.range(1.0, 6.0),
                (i / 4) as f32 * 2.0 - 2.0,
            );
            random_body(rng, at)
        })
        .collect()
}

/// A 10 m thick static floor whose top is y = 0, 400 m wide.
fn spawn_floor(app: &mut App) {
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -5.0, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(200.0, 5.0, 200.0),
            },
            friction: 0.6,
            ..Collider::default()
        },
    ));
}

fn spawn_scene(app: &mut App, scene: &[BodySpec]) -> Vec<Entity> {
    spawn_floor(app);
    scene
        .iter()
        .map(|b| {
            app.world_mut()
                .spawn((
                    Transform {
                        translation: b.at,
                        rotation: b.rotation,
                        ..Transform::default()
                    },
                    RigidBody::dynamic(),
                    Collider {
                        shape: b.shape.clone(),
                        friction: b.friction,
                        restitution: 0.0,
                        ..Collider::default()
                    },
                    Velocity {
                        linear: b.linear,
                        angular: b.angular,
                    },
                ))
                .id()
        })
        .collect()
}

/// Kinetic + gravitational energy of the dynamic bodies (J), from rapier's own mass
/// properties.
fn mechanical_energy(app: &App) -> f32 {
    let world = app.world().resource::<PhysicsWorld>();
    let gravity = rapier3d::math::Vector::new(0.0, -9.81, 0.0);
    world
        .bodies
        .iter()
        .filter(|(_, b)| b.is_dynamic())
        .map(|(_, b)| b.kinetic_energy() + b.gravitational_potential_energy(1.0 / 60.0, gravity))
        .sum()
}

fn all_finite(app: &App, entities: &[Entity]) -> bool {
    entities.iter().all(|&e| {
        let pose_ok = app
            .world()
            .get::<PhysicsPose>(e)
            .is_none_or(|p| p.translation.is_finite() && p.rotation.is_finite());
        let velocity_ok = app
            .world()
            .get::<Velocity>(e)
            .is_none_or(|v| v.linear.is_finite() && v.angular.is_finite());
        pose_ok && velocity_ok
    })
}

/// Updates until the rapier world holds `bodies` bodies (the first fixed step creates them).
fn update_until_created(app: &mut App, bodies: usize) {
    for _ in 0..8 {
        app.update();
        if app.world().resource::<PhysicsWorld>().bodies.len() >= bodies {
            return;
        }
    }
    panic!("the scene's {bodies} bodies were not created within 8 frames");
}

/// Random drops: finite state at every step, everything above the floor at the end, and no
/// energy gained over 4 s.
#[test]
fn property_random_drops_stay_finite_above_the_floor_and_lose_energy() {
    let mut rng = XorShift(0x0DDB_A110_FF00_D00D);
    for case in 0..24 {
        let scene = random_scene(&mut rng);
        let mut app = physics_app();
        let entities = spawn_scene(&mut app, &scene);
        // Measure the energy the bodies start with, once the first step created them.
        update_until_created(&mut app, scene.len() + 1);
        let start = mechanical_energy(&app);
        for step in 0..240 {
            app.update();
            assert!(
                all_finite(&app, &entities),
                "case {case}, step {step}: a pose or velocity is not finite\n{scene:#?}"
            );
        }
        let end = mechanical_energy(&app);
        assert!(
            end <= start + 0.02 * start.abs() + 0.1,
            "case {case}: energy grew from {start} J to {end} J\n{scene:#?}"
        );
        for (spec, &e) in scene.iter().zip(&entities) {
            let y = app
                .world()
                .get::<PhysicsPose>(e)
                .expect("stepped pose")
                .translation
                .y;
            assert!(
                y >= spec.min_extent - 0.05,
                "case {case}: a body sank into the floor (y={y}, extent {})\n{spec:#?}",
                spec.min_extent
            );
        }
    }
}

/// The same random scene simulated twice is bit-identical at every step.
#[test]
fn property_random_scenes_replay_bit_identically() {
    let mut rng = XorShift(0x7E57_AB1E_5EED_0001);
    for case in 0..8 {
        let scene = random_scene(&mut rng);
        let mut a = physics_app();
        let mut b = physics_app();
        spawn_scene(&mut a, &scene);
        spawn_scene(&mut b, &scene);
        for step in 0..120 {
            a.update();
            b.update();
            let da = a.world().resource::<PhysicsWorld>().state_digest();
            let db = b.world().resource::<PhysicsWorld>().state_digest();
            assert_eq!(da, db, "case {case} diverged at step {step}\n{scene:#?}");
        }
    }
}

/// Generated ball-joint chains (2–8 links, random sizes, gaps and kicks) stay connected.
#[test]
fn property_random_chains_stay_connected() {
    let mut rng = XorShift(0xC4A1_4C4A_1400_0003);
    for case in 0..12 {
        let mut app = physics_app();
        let links = rng.int(2, 8) as usize;
        let half = rng.range(0.05, 0.2);
        let gap = rng.range(0.05, 0.3);
        let reach = half + gap * 0.5; // center to joint point
        let top = Vec3::new(0.0, 20.0, 0.0);
        let anchor = app
            .world_mut()
            .spawn((
                Transform::from_translation(top),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(0.1),
                    },
                    ..Collider::default()
                },
            ))
            .id();
        let mut joints = Vec::new();
        let mut parent = anchor;
        let mut parent_reach = 0.1 + gap * 0.5;
        let mut y = top.y - parent_reach - reach;
        let kick = rng.vec(3.0);
        for i in 0..links {
            let link = app
                .world_mut()
                .spawn((
                    Transform::from_translation(Vec3::new(0.0, y, 0.0)),
                    RigidBody::dynamic(),
                    Collider {
                        shape: ColliderShape::Box {
                            half_extents: Vec3::splat(half),
                        },
                        ..Collider::default()
                    },
                    Velocity {
                        linear: if i + 1 == links { kick } else { Vec3::ZERO },
                        angular: Vec3::ZERO,
                    },
                ))
                .id();
            // Ragdoll practice: jointed neighbors do not collide (their corners meet when
            // the chain folds).
            app.world_mut().entity_mut(parent).insert((
                Joint {
                    kind: JointKind::Spherical { cone_limit: None },
                    connected: link,
                    anchor: Vec3::new(0.0, -parent_reach, 0.0),
                    connected_anchor: Vec3::new(0.0, reach, 0.0),
                    break_force: 0.0,
                },
                JointNoContacts,
            ));
            joints.push(parent);
            parent = link;
            parent_reach = reach;
            y -= 2.0 * reach;
        }

        let mut worst = 0.0f32;
        for _ in 0..120 {
            app.update();
            for &j in &joints {
                let joint = app.world().get::<Joint>(j).expect("joint").clone();
                let pose_of = |e: Entity| -> (Vec3, Quat) {
                    app.world().get::<PhysicsPose>(e).map_or_else(
                        || {
                            let t = app.world().get::<Transform>(e).expect("transform");
                            (t.translation, t.rotation)
                        },
                        |p| (p.translation, p.rotation),
                    )
                };
                let (pa, ra) = pose_of(j);
                let (pb, rb) = pose_of(joint.connected);
                let gap_now = ((pa + ra * joint.anchor) - (pb + rb * joint.connected_anchor)).length();
                worst = worst.max(gap_now);
            }
        }
        let live = app.world().resource::<PhysicsWorld>().impulse_joints.len();
        assert_eq!(live, links, "case {case}: every joint exists");
        assert!(
            worst < 0.1,
            "case {case}: a joint opened by {worst} m (links {links}, half {half}, gap {gap}, kick {kick:?})"
        );
        // Every link has a rapier body (nothing was dropped on the way).
        let bodies = app.world().resource::<PhysicsWorld>().bodies.len();
        assert_eq!(bodies, links + 1, "case {case}");
        assert!(
            app.world().get::<RigidBodyHandle>(anchor).is_some(),
            "case {case}"
        );
    }
}
