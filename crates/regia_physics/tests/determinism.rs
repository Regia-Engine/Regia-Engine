// SPDX-License-Identifier: MPL-2.0
//! Determinism (specs/15 § 5, ADR-870): the same build, platform and inputs give a
//! bit-identical simulation, whatever the thread count, the executor and the process.
//!
//! The scene is a scripted "churn" run: bodies fall and pile up, some are despawned, one
//! loses and regains its `RigidBody`, a joint is edited and another breaks, forces come and
//! go and new bodies arrive mid-run. Each run folds [`PhysicsWorld::state_digest`] of every
//! step into one value, and runs are compared on that value.
//!
//! - Two apps in one process: every `HashMap` gets its own random seed, so any simulation
//!   order taken from a hash map (removal order, joint order) shows up as a mismatch.
//! - Single-threaded vs multi-threaded executor.
//! - 1, 2, 4 and 8 compute threads: the global task pools are sized once per process, so
//!   the test re-runs this test binary as a child process per thread count (the ignored
//!   `determinism_worker` test) and compares the digests the children print.
//!
//! Provenance: FNV-1a digest (public domain); scripted scene and xorshift generator are
//! Regia Engine's own.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::ecs::schedule::SingleThreadedExecutor;
use regia_ecs::prelude::{Entity, Schedules};
use regia_physics::{
    Collider, ColliderShape, ExternalForce, Joint, JointKind, PhysicsPlugin, PhysicsWorld, RigidBody,
    Velocity,
};
use regia_scene::Transform;

/// Steps in one churn run.
const STEPS: usize = 300;

/// Environment variable that turns `determinism_worker` on and sets its thread count.
const THREADS_ENV: &str = "REGIA_PHYSICS_DETERMINISM_THREADS";

/// Line prefix the worker prints its digest after.
const DIGEST_PREFIX: &str = "REGIA_PHYSICS_DIGEST ";

/// Tiny deterministic generator (xorshift64; no dependency needed).
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

fn spawn_body(app: &mut App, rng: &mut XorShift, at: Vec3) -> Entity {
    let shape = if rng.next().is_multiple_of(3) {
        ColliderShape::Sphere {
            radius: rng.range(0.15, 0.4),
        }
    } else {
        ColliderShape::Box {
            half_extents: Vec3::new(rng.range(0.15, 0.4), rng.range(0.15, 0.4), rng.range(0.15, 0.4)),
        }
    };
    let rotation = Quat::from_euler(
        regia_core::math::EulerRot::XYZ,
        rng.range(-3.0, 3.0),
        rng.range(-3.0, 3.0),
        rng.range(-3.0, 3.0),
    );
    app.world_mut()
        .spawn((
            Transform {
                translation: at,
                rotation,
                ..Transform::default()
            },
            RigidBody::dynamic(),
            Collider {
                shape,
                friction: rng.range(0.2, 1.0),
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(rng.range(-3.0, 3.0), rng.range(-1.0, 1.0), rng.range(-3.0, 3.0)),
                angular: Vec3::new(rng.range(-2.0, 2.0), rng.range(-2.0, 2.0), rng.range(-2.0, 2.0)),
            },
        ))
        .id()
}

/// Runs the scripted churn scene on `app` and folds every step's state digest.
fn run_churn(mut app: App) -> u64 {
    let mut rng = XorShift(0x5EED_F00D_CAFE_D00D);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(30.0, 0.5, 30.0),
            },
            ..Collider::default()
        },
    ));
    // Two layers of 6×6 bodies, 1.2 m apart (no initial overlap: extents ≤ 0.7 m).
    let mut bodies = Vec::new();
    for layer in 0..2 {
        for i in 0..6 {
            for k in 0..6 {
                let at = Vec3::new(
                    i as f32 * 1.2 - 3.0,
                    1.5 + layer as f32 * 1.6,
                    k as f32 * 1.2 - 3.0,
                );
                bodies.push(spawn_body(&mut app, &mut rng, at));
            }
        }
    }
    // A hanging chain of four links; the last joint is breakable.
    let anchor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(8.0, 6.0, 0.0)),
            RigidBody::static_body(),
            Collider::default(),
        ))
        .id();
    let mut parent = anchor;
    let mut joint_entities = Vec::new();
    for i in 0..4 {
        let link = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(8.0, 5.0 - i as f32, 0.0)),
                RigidBody::dynamic(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::splat(0.2),
                    },
                    ..Collider::default()
                },
                Velocity {
                    linear: Vec3::new(1.0 + i as f32, 0.0, 0.5),
                    angular: Vec3::ZERO,
                },
            ))
            .id();
        app.world_mut().entity_mut(parent).insert(Joint {
            kind: JointKind::Spherical { cone_limit: None },
            connected: link,
            anchor: Vec3::new(0.0, -0.5, 0.0),
            connected_anchor: Vec3::new(0.0, 0.5, 0.0),
            break_force: if i == 3 { 1.0 } else { 0.0 },
        });
        joint_entities.push(parent);
        parent = link;
    }

    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for step in 0..STEPS {
        match step {
            60 => {
                for &e in bodies.iter().step_by(7) {
                    app.world_mut().despawn(e);
                }
            }
            90 => {
                app.world_mut().entity_mut(bodies[10]).remove::<RigidBody>();
            }
            100 => {
                app.world_mut()
                    .entity_mut(bodies[10])
                    .insert(RigidBody::dynamic());
            }
            120 => {
                if let Some(mut joint) = app.world_mut().get_mut::<Joint>(joint_entities[1]) {
                    joint.kind = JointKind::Revolute {
                        axis: Vec3::Z,
                        limits: Some((-0.6, 0.6)),
                        motor: None,
                    };
                }
            }
            130 => {
                app.world_mut().entity_mut(bodies[12]).insert(ExternalForce {
                    force: Vec3::new(4.0, 6.0, 0.0),
                    torque: Vec3::new(0.0, 0.3, 0.0),
                });
            }
            150 => {
                for i in 0..5 {
                    let at = Vec3::new(i as f32 * 1.5 - 3.0, 6.0, 4.0);
                    bodies.push(spawn_body(&mut app, &mut rng, at));
                }
            }
            200 => {
                app.world_mut().entity_mut(bodies[12]).remove::<ExternalForce>();
            }
            _ => {}
        }
        app.update();
        let digest = app.world().resource::<PhysicsWorld>().state_digest();
        fold ^= digest;
        fold = fold.wrapping_mul(0x0000_0100_0000_01b3);
    }
    fold
}

/// A pile whose colliders all change on one tick, with or without a physics-neutral marker on
/// every other body (which reorders the archetypes the change query walks). Folds every step's
/// digest.
fn run_collider_rebuilds(shuffle_archetypes: bool) -> u64 {
    let mut app = physics_app();
    let mut rng = XorShift(0xC011_1DE5_0000_0001);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));
    let mut bodies = Vec::new();
    for i in 0..4 {
        for k in 0..4 {
            let at = Vec3::new(i as f32 * 0.9 - 1.4, 0.5 + k as f32 * 0.9, 0.0);
            bodies.push(spawn_body(&mut app, &mut rng, at));
        }
    }
    if shuffle_archetypes {
        for &e in bodies.iter().step_by(2) {
            app.world_mut()
                .entity_mut(e)
                .insert(regia_scene::components::Name::new("marked"));
        }
    }
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for step in 0..120 {
        if step == 40 {
            for &e in &bodies {
                if let Some(mut collider) = app.world_mut().get_mut::<Collider>(e) {
                    collider.friction = (collider.friction + 0.25).min(1.0);
                }
            }
        }
        app.update();
        fold ^= app.world().resource::<PhysicsWorld>().state_digest();
        fold = fold.wrapping_mul(0x0000_0100_0000_01b3);
    }
    fold
}

/// A body's spawn rotation must reach rapier as the quaternion's exact bits (spec 15 § 5):
/// routing the rotation through the axis-angle vector (`atan2`, then rapier's `sin`/`cos`)
/// rounds differently on each platform libm, so `state_digest` would disagree on the spawn
/// step of a lockstep run with `physics-cross-platform-determinism` on.
#[test]
fn spawn_rotation_bits_reach_rapier_unchanged() {
    let mut app = physics_app();
    // 0.1023 about Y: the axis-angle round trip (atan2, then sin/cos) lands one ulp away
    // from the authored quaternion on this platform's libm; the pose must carry the bits.
    let rotation = Quat::from_rotation_y(0.1023);
    app.world_mut().spawn((
        Transform {
            translation: Vec3::new(0.0, 3.0, 0.0),
            rotation,
            ..Transform::default()
        },
        // No gravity and no initial velocity: the step does not touch the pose (rapier would
        // renormalize it, a different, deterministic operation — the libm round trip this
        // test pins is in the spawn conversion).
        RigidBody {
            gravity_scale: 0.0,
            ..RigidBody::dynamic()
        },
        Collider::default(),
    ));
    // One update: the body sync runs, and with zero gravity and zero velocity the step does
    // not touch the pose (rapier would renormalize it, which is a different, deterministic
    // operation — the libm round trip this test pins is in the spawn conversion).
    app.update();
    let world = app.world().resource::<PhysicsWorld>();
    let (_, rb) = world.bodies.iter().next().expect("the spawned body");
    let r = rb.position().rotation;
    let stored = [r.x.to_bits(), r.y.to_bits(), r.z.to_bits(), r.w.to_bits()];
    let authored = [
        rotation.x.to_bits(),
        rotation.y.to_bits(),
        rotation.z.to_bits(),
        rotation.w.to_bits(),
    ];
    // What must NOT be in the stored bits is the axis-angle round trip: `atan2` then
    // `sin`/`cos`, whose value depends on the platform libm (pre-fix this test failed).
    let round_trip =
        rapier3d::math::Rotation::from_scaled_axis(regia_physics::convert::to_rapier_angvec(rotation));
    let round_trip = [
        round_trip.x.to_bits(),
        round_trip.y.to_bits(),
        round_trip.z.to_bits(),
        round_trip.w.to_bits(),
    ];
    assert_ne!(
        stored, round_trip,
        "rapier must not store the atan2/sin/cos round trip"
    );
    // Depending on the build, the step stores the authored quaternion as-is or renormalized
    // once (glamx pose composition; `sqrt`/`/`, exact IEEE ops, bit-identical on every
    // target). Both are the authored rotation; the round trip above is not.
    let renormalized = rotation.normalize();
    let renormalized = [
        renormalized.x.to_bits(),
        renormalized.y.to_bits(),
        renormalized.z.to_bits(),
        renormalized.w.to_bits(),
    ];
    assert!(
        stored == authored || stored == renormalized,
        "rapier must store the authored quaternion (maybe renormalized), got {stored:08x?}, \
         authored {authored:08x?}, renormalized {renormalized:08x?}"
    );
}

/// Collider rebuilds follow entity order, not archetype order (ADR-1660): an archetype history
/// that differs between two runs (one more component on some bodies) gives the same simulation.
#[test]
fn collider_rebuilds_do_not_depend_on_archetype_order() {
    assert_eq!(run_collider_rebuilds(false), run_collider_rebuilds(true));
}

/// Two apps in one process (different hash seeds) produce the same run.
#[test]
fn two_runs_of_a_churning_scene_are_bit_identical() {
    let first = run_churn(physics_app());
    let second = run_churn(physics_app());
    assert_eq!(first, second, "the same inputs gave different simulations");
}

/// The single-threaded executor produces the same run as the default (multi-threaded) one.
#[test]
fn single_and_multi_threaded_executors_agree() {
    let multi = run_churn(physics_app());
    let mut single = physics_app();
    {
        let mut schedules = single.world_mut().resource_mut::<Schedules>();
        for (_, schedule) in schedules.iter_mut() {
            schedule.set_executor(SingleThreadedExecutor::new());
        }
    }
    assert_eq!(run_churn(single), multi, "the executor changed the simulation");
}

/// Worker for [`digest_is_identical_across_compute_thread_counts`]: sizes the compute pool
/// from `REGIA_PHYSICS_DETERMINISM_THREADS`, runs the churn scene and prints its digest.
/// Without the variable it does nothing (so `--include-ignored` runs stay green).
#[test]
#[ignore = "child-process worker of digest_is_identical_across_compute_thread_counts"]
fn determinism_worker() {
    let Ok(threads) = regia_core::brand::env_var(THREADS_ENV) else {
        return;
    };
    let threads: usize = threads.parse().expect("thread count");
    let sizes = regia_core::tasks::init(regia_core::tasks::TaskPoolOptions {
        compute_threads: Some(threads),
        ..regia_core::tasks::TaskPoolOptions::default()
    });
    assert_eq!(sizes.compute, threads, "the worker sized the compute pool first");
    let digest = run_churn(physics_app());
    println!("{DIGEST_PREFIX}{digest:016x}");
}

/// 1, 2, 4 and 8 compute threads give the same run (each in its own process, since the
/// pools are global).
#[test]
fn digest_is_identical_across_compute_thread_counts() {
    let exe = std::env::current_exe().expect("path of this test binary");
    let mut digests = Vec::new();
    for threads in [1usize, 2, 4, 8] {
        let output = std::process::Command::new(&exe)
            .args([
                "determinism_worker",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(THREADS_ENV, threads.to_string())
            .output()
            .expect("start the worker process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "worker with {threads} threads failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // libtest writes `test determinism_worker ... ` without a newline before the test's
        // own output, so the digest can follow it on the same line.
        let digest = stdout
            .lines()
            .find_map(|line| line.split_once(DIGEST_PREFIX).map(|(_, rest)| rest))
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("worker with {threads} threads printed no digest:\n{stdout}"));
        digests.push((threads, digest));
    }
    let in_process = format!("{:016x}", run_churn(physics_app()));
    for (threads, digest) in &digests {
        assert_eq!(
            digest, &in_process,
            "{threads} compute threads diverged from this process's run: {digests:?}"
        );
    }
}
