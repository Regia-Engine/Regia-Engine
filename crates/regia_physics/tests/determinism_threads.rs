// SPDX-License-Identifier: MPL-2.0
//! Physics determinism across thread counts (specs/15 § 5, ADR-541).
//!
//! One busy scene (a jittered pile of boxes, a revolute chain, a cone-limited pendulum, a CCD
//! bullet, a moving kinematic platform, a trigger volume and joints that break) is stepped
//! a fixed number of times and fingerprinted: every body's pose and velocity bits plus every
//! physics message in delivery order. The fingerprint must not change with:
//!
//! - a second run in the same process;
//! - the schedule executor (single-threaded vs multi-threaded);
//! - running four simulations at once on four OS threads;
//! - the compute pool size (1, 2, 4 and 8 threads), each in its own child process, because
//!   the pools are process-global and sized once.
//!
//! The child processes re-run this test binary with `--exact determinism_child` and
//! `REGIA_PHYSICS_DET_THREADS` set; the child prints its fingerprint on one line.
//!
//! Provenance: FNV-1a (Fowler, Noll, Vo; public domain description) for the fingerprint; the
//! scene and the comparison method are Regia Engine's.

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use regia_app::{App, FixedTime, FixedUpdate, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::ecs::message::{Message, MessageCursor, Messages};
use regia_ecs::ecs::schedule::{MultiThreadedExecutor, Schedules, SingleThreadedExecutor};
use regia_ecs::prelude::*;
use regia_physics::{
    ActiveEvents, AuthoredCollision, Collider, ColliderShape, CollisionEnded, CollisionStarted, Joint,
    JointBroken, JointKind, JointNoContacts, PhysicsPlugin, PhysicsStep, PhysicsWorld, QueryFilter,
    RigidBody, RigidBodyHandle, TriggerEnter, TriggerExit, Velocity, WorldQueries,
};
use regia_scene::Transform;

/// Child-process switch: the compute pool size to run the scene with.
const THREADS_ENV: &str = "REGIA_PHYSICS_DET_THREADS";
/// Prefix of the child's fingerprint line on stdout.
const FINGERPRINT_PREFIX: &str = "REGIA_PHYSICS_FINGERPRINT=";
/// Fixed steps per run (5 s at 60 Hz).
const STEPS: usize = 300;

// ---------------------------------------------------------------------------
// The fingerprint
// ---------------------------------------------------------------------------

/// FNV-1a over 32-bit words.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn word(&mut self, value: u32) {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn floats(&mut self, values: &[f32]) {
        for v in values {
            self.word(v.to_bits());
        }
    }

    fn entity(&mut self, entity: Entity) {
        self.word(entity.index().index());
    }
}

/// Folds every new message of type `M` into the hash, tagged so different kinds never alias.
fn hash_messages<M: Message>(
    app: &App,
    cursor: &mut MessageCursor<M>,
    hash: &mut Fnv,
    tag: u32,
    entities: impl Fn(&M) -> [Entity; 2],
) {
    for message in cursor.read(app.world().resource::<Messages<M>>()) {
        hash.word(tag);
        for entity in entities(message) {
            hash.entity(entity);
        }
    }
}

// ---------------------------------------------------------------------------
// The scene
// ---------------------------------------------------------------------------

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

fn boxed(half: Vec3) -> Collider {
    Collider {
        shape: ColliderShape::Box { half_extents: half },
        friction: 0.7,
        active_events: ActiveEvents {
            collision: true,
            contact_force: false,
        },
        ..Collider::default()
    }
}

/// Builds the scene and returns the entities to fingerprint, in spawn order.
fn build_scene(app: &mut App) -> Vec<Entity> {
    let world = app.world_mut();
    let mut bodies = Vec::new();

    // Ground (top at y = 0).
    world.spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        boxed(Vec3::new(20.0, 0.5, 20.0)),
    ));

    // A jittered pile: 3 layers of 4×4 boxes, offsets from an integer hash (no RNG state).
    for layer in 0..3u32 {
        for row in 0..4u32 {
            for col in 0..4u32 {
                let n = layer * 16 + row * 4 + col;
                let jitter = |salt: u32| ((n.wrapping_mul(2_654_435_761) ^ salt) % 97) as f32 / 97.0 - 0.5;
                let p = Vec3::new(
                    col as f32 * 0.55 - 0.8 + jitter(1) * 0.1,
                    0.3 + layer as f32 * 0.7 + jitter(2) * 0.05,
                    row as f32 * 0.55 - 0.8 + jitter(3) * 0.1,
                );
                bodies.push(
                    world
                        .spawn((
                            Transform::from_translation(p),
                            RigidBody::dynamic(),
                            boxed(Vec3::splat(0.25)),
                            Velocity::default(),
                        ))
                        .id(),
                );
            }
        }
    }

    // A six-link revolute chain from a static anchor, kicked sideways.
    let anchor = world
        .spawn((
            Transform::from_translation(Vec3::new(6.0, 5.0, 0.0)),
            RigidBody::static_body(),
        ))
        .id();
    let mut previous = anchor;
    for i in 0..6 {
        let link = world
            .spawn((
                Transform::from_translation(Vec3::new(6.0, 4.5 - i as f32 * 0.5, 0.0)),
                RigidBody::dynamic(),
                boxed(Vec3::splat(0.1)),
                Velocity {
                    linear: Vec3::new(if i == 5 { 3.0 } else { 0.0 }, 0.0, 0.0),
                    angular: Vec3::ZERO,
                },
            ))
            .id();
        world.entity_mut(previous).insert((
            Joint {
                kind: JointKind::Revolute {
                    axis: Vec3::Z,
                    limits: None,
                    motor: None,
                },
                connected: link,
                anchor: Vec3::new(0.0, -0.25, 0.0),
                connected_anchor: Vec3::new(0.0, 0.25, 0.0),
                break_force: 0.0,
            },
            JointNoContacts,
        ));
        bodies.push(link);
        previous = link;
    }

    // A cone-limited pendulum, kicked diagonally.
    let socket = world
        .spawn((
            Transform::from_translation(Vec3::new(-6.0, 4.0, 0.0)),
            RigidBody::static_body(),
        ))
        .id();
    let rod = world
        .spawn((
            Transform::from_translation(Vec3::new(-6.0, 3.5, 0.0)),
            RigidBody::dynamic(),
            boxed(Vec3::new(0.05, 0.5, 0.05)),
            Velocity {
                linear: Vec3::new(1.5, 0.0, 1.0),
                angular: Vec3::ZERO,
            },
        ))
        .id();
    world.entity_mut(socket).insert(Joint {
        kind: JointKind::Spherical {
            cone_limit: Some(0.4),
        },
        connected: rod,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::new(0.0, 0.5, 0.0),
        break_force: 0.0,
    });
    bodies.push(rod);

    // Two weak fixed joints holding planks: both break on the first loaded step.
    for x in [-3.0f32, -2.0] {
        let hook = world
            .spawn((
                Transform::from_translation(Vec3::new(x, 6.0, 4.0)),
                RigidBody::static_body(),
            ))
            .id();
        let plank = world
            .spawn((
                Transform::from_translation(Vec3::new(x, 5.5, 4.0)),
                RigidBody::dynamic(),
                boxed(Vec3::new(0.3, 0.05, 0.3)),
            ))
            .id();
        world.entity_mut(hook).insert(Joint {
            kind: JointKind::Fixed,
            connected: plank,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::new(0.0, 0.5, 0.0),
            break_force: 0.001,
        });
        bodies.push(plank);
    }

    // A CCD bullet fired into the pile.
    bodies.push(
        world
            .spawn((
                Transform::from_translation(Vec3::new(-8.0, 0.6, 0.1)),
                RigidBody {
                    ccd: true,
                    gravity_scale: 0.0,
                    ..RigidBody::dynamic()
                },
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.08 },
                    density: 50.0,
                    ..Collider::default()
                },
                Velocity {
                    linear: Vec3::new(120.0, 0.0, 0.0),
                    angular: Vec3::ZERO,
                },
            ))
            .id(),
    );

    // A trigger volume the pile spills into.
    world.spawn((
        Transform::from_translation(Vec3::new(2.5, 0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            sensor: true,
            ..boxed(Vec3::new(1.0, 0.5, 2.0))
        },
    ));

    // A kinematic platform that slides back and forth under a passenger.
    let platform = world
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.25, 8.0)),
            RigidBody::kinematic(),
            boxed(Vec3::new(1.5, 0.25, 1.5)),
        ))
        .id();
    bodies.push(platform);
    bodies.push(
        world
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.85, 8.0)),
                RigidBody::dynamic(),
                boxed(Vec3::splat(0.3)),
                Velocity::default(),
            ))
            .id(),
    );

    // Drive the platform before the physics step: 1 m/s, reversing every 2 s.
    app.add_systems(
        FixedUpdate,
        (move |mut transforms: Query<&mut Transform>, mut step: Local<u32>| {
            *step += 1;
            let direction = if (*step / 120).is_multiple_of(2) {
                1.0
            } else {
                -1.0
            };
            if let Ok(mut t) = transforms.get_mut(platform) {
                t.translation.x += direction / 60.0;
            }
        })
        .before(PhysicsStep),
    );

    bodies
}

/// Runs the scene for `STEPS` fixed steps and returns its fingerprint.
fn run_scene(configure: impl FnOnce(&mut App)) -> u64 {
    let mut app = physics_app();
    configure(&mut app);
    let bodies = build_scene(&mut app);

    let mut hash = Fnv::new();
    let mut started = MessageCursor::<CollisionStarted>::default();
    let mut ended = MessageCursor::<CollisionEnded>::default();
    let mut enter = MessageCursor::<TriggerEnter>::default();
    let mut exit = MessageCursor::<TriggerExit>::default();
    let mut broken = MessageCursor::<JointBroken>::default();
    for _ in 0..STEPS {
        app.update();
        hash_messages(&app, &mut started, &mut hash, 1, |m| [m.entity_a, m.entity_b]);
        hash_messages(&app, &mut ended, &mut hash, 2, |m| [m.entity_a, m.entity_b]);
        hash_messages(&app, &mut enter, &mut hash, 3, |m| [m.trigger, m.other]);
        hash_messages(&app, &mut exit, &mut hash, 4, |m| [m.trigger, m.other]);
        hash_messages(&app, &mut broken, &mut hash, 5, |m| [m.entity, m.connected]);
    }

    let world = app.world();
    for &entity in &bodies {
        let t = world.get::<Transform>(entity).copied().unwrap_or_default();
        hash.floats(&t.translation.to_array());
        hash.floats(&t.rotation.to_array());
        let v = world.get::<Velocity>(entity).cloned().unwrap_or_default();
        hash.floats(&v.linear.to_array());
        hash.floats(&v.angular.to_array());
        let handle = world.get::<RigidBodyHandle>(entity).map_or(u32::MAX, |h| h.raw);
        hash.word(handle);
    }
    hash.0
}

/// Runs every schedule of the app on the single-threaded executor.
fn use_single_threaded_executor(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
}

/// Runs every schedule of the app on the multi-threaded executor.
fn use_multi_threaded_executor(app: &mut App) {
    let mut schedules = app.world_mut().resource_mut::<Schedules>();
    for (_, schedule) in schedules.iter_mut() {
        schedule.set_executor(MultiThreadedExecutor::new());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn same_scene_twice_is_bit_identical() {
    let first = run_scene(|_| {});
    let second = run_scene(|_| {});
    assert_eq!(first, second, "two runs in one process diverged");
}

#[test]
fn single_and_multi_threaded_executors_agree() {
    let single = run_scene(use_single_threaded_executor);
    let multi = run_scene(use_multi_threaded_executor);
    assert_eq!(single, multi, "the executor kind changed the simulation");
}

#[test]
fn four_concurrent_simulations_match_a_serial_run() {
    let serial = run_scene(|_| {});
    let concurrent: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| run_scene(|_| {}))).collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("simulation thread"))
            .collect()
    });
    for (i, fingerprint) in concurrent.iter().enumerate() {
        assert_eq!(
            *fingerprint, serial,
            "concurrent run {i} diverged from the serial run"
        );
    }
}

/// The child half of `compute_pool_size_does_not_change_the_simulation`: a no-op unless
/// the parent set `REGIA_PHYSICS_DET_THREADS`.
#[test]
fn determinism_child() {
    let Ok(threads) = regia_core::brand::env_var(THREADS_ENV) else {
        return;
    };
    let threads: usize = threads.parse().expect("thread count");
    let sizes = regia_core::tasks::init(regia_core::tasks::TaskPoolOptions {
        compute_threads: Some(threads),
        ..regia_core::tasks::TaskPoolOptions::default()
    });
    assert_eq!(sizes.compute, threads, "the child sized its compute pool first");
    println!("{FINGERPRINT_PREFIX}{:016x}", run_scene(|_| {}));
}

#[test]
fn compute_pool_size_does_not_change_the_simulation() {
    if regia_core::brand::env_var(THREADS_ENV).is_ok() {
        return; // inside a child: only `determinism_child` runs there
    }
    let exe = std::env::current_exe().expect("test executable");
    let mut fingerprints = Vec::new();
    for threads in [1usize, 2, 4, 8] {
        let output = Command::new(&exe)
            .args(["--exact", "determinism_child", "--nocapture", "--test-threads=1"])
            .env(THREADS_ENV, threads.to_string())
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child with {threads} threads failed: {}\n{stdout}",
            String::from_utf8_lossy(&output.stderr)
        );
        // libtest writes `test determinism_child ... ` without a newline before the test's own
        // output, so the fingerprint can follow it on the same line.
        let fingerprint = stdout
            .lines()
            .find_map(|line| line.split_once(FINGERPRINT_PREFIX).map(|(_, rest)| rest))
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("child with {threads} threads printed no fingerprint:\n{stdout}"))
            .to_owned();
        fingerprints.push((threads, fingerprint));
    }
    let (_, reference) = &fingerprints[0];
    for (threads, fingerprint) in &fingerprints {
        assert_eq!(
            fingerprint, reference,
            "{threads} compute threads diverged from 1 thread"
        );
    }
}

/// Two joints breaking on the same step are reported in entity order even when the query
/// visits them the other way round (archetype order), so replays see the same messages.
#[test]
fn simultaneous_joint_breaks_are_reported_in_entity_order() {
    let mut app = physics_app();
    let mut hooks = Vec::new();
    for x in [0.0f32, 3.0] {
        let world = app.world_mut();
        let hook = world
            .spawn((
                Transform::from_translation(Vec3::new(x, 5.0, 0.0)),
                RigidBody::static_body(),
            ))
            .id();
        let plank = world
            .spawn((
                Transform::from_translation(Vec3::new(x, 4.5, 0.0)),
                RigidBody::dynamic(),
                Collider::default(),
            ))
            .id();
        world.entity_mut(hook).insert(Joint {
            kind: JointKind::Fixed,
            connected: plank,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::new(0.0, 0.5, 0.0),
            break_force: 0.0,
        });
        hooks.push(hook);
    }
    let (first, second) = (hooks[0], hooks[1]);
    assert!(first.index().index() < second.index().index());
    app.update();
    app.update();

    // Move the first hook into an archetype created after the second's, so a joint query
    // now visits the second hook first; then make both joints break on the next step.
    app.world_mut().entity_mut(first).insert(Velocity::default());
    for hook in [first, second] {
        app.world_mut().get_mut::<Joint>(hook).expect("joint").break_force = 0.001;
    }

    let mut cursor = MessageCursor::<JointBroken>::default();
    let mut order = Vec::new();
    for _ in 0..10 {
        app.update();
        let messages = app.world().resource::<Messages<JointBroken>>();
        order.extend(cursor.read(messages).map(|m| m.entity));
    }
    assert_eq!(order, vec![first, second], "breaks reported in entity order");
}

// ---------------------------------------------------------------------------
// Authored collision attach order (ADR-1347)
// ---------------------------------------------------------------------------

/// Temp project with one cooked collision set (`assets/props/crate.glb`, one 1 m box proxy),
/// enough for `AuthoredCollision` to attach a collider. Unique per call (pid + counter).
fn authored_collision_fixture() -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "regia-physics-authored-det-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let cache = regia_asset::cooked_cache_dir(&root, &regia_asset::current_platform());
    std::fs::create_dir_all(&cache).expect("cache dir");
    // The cooked file lives under the set's sub-asset id (`<glTF>/collision`), which is what
    // `load_authored_collision` reads; the manifest maps the path to the parent glTF id.
    let parent = regia_asset::AssetId::sub_asset(regia_asset::AssetId::default(), "det-fixture");
    let id = regia_asset::AssetId::sub_asset(parent, regia_asset::COLLISION_SET_SUB_PATH);
    let set = regia_asset::CookedCollisionSet::new(vec![regia_asset::CookedCollisionShape {
        node: "collision_box_crate".into(),
        requested: regia_asset::CollisionProxyKind::Box,
        translation: [0.0; 3],
        rotation: [0.0, 0.0, 0.0, 1.0],
        geometry: regia_asset::CookedCollisionGeometry::Box(regia_asset::collision_set::CollisionBox {
            half_extents: [0.5, 0.5, 0.5],
        }),
    }]);
    let payload = set.to_postcard().expect("postcard");
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_asset::AssetTypeTag::collision_set().0,
        asset_id: id,
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let file = regia_asset::encode_cooked_file(&header, &payload).expect("cooked file");
    std::fs::write(cache.join(regia_asset::cooked_file_name(id)), file).expect("cooked file write");
    let manifest = regia_asset::AssetManifest {
        entries: vec![regia_asset::ManifestEntry {
            path: "assets/props/crate.glb".into(),
            id: parent,
            type_path: regia_asset::COLLISION_SET_TYPE_PATH.into(),
        }],
    };
    manifest.write_to_cache(&cache).expect("manifest write");
    root
}

/// Two dynamic bodies whose colliders come from the same `AuthoredCollision` proxy, dropped
/// overlapping so they push each other while settling on the floor. With `invert_ids`, both
/// entity ids are reserved first (as a parallel system's earlier-recorded command queue
/// would reserve them) and the components are applied left-before-right, so the entity-id
/// order is the reverse of the component arrival order — what another thread count produces
/// (ADR-1347). The rapier insertion order must follow arrival order in both runs.
/// Returns the step digests and the `CollisionStarted` sequence with entities mapped to
/// their scene role (the ids themselves legitimately differ between the two timings).
fn authored_digests(invert_ids: bool) -> (Vec<u64>, Vec<(&'static str, &'static str)>) {
    let root = authored_collision_fixture();
    let mut app = physics_app();
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.clone(),
        platform: regia_asset::current_platform(),
        content_dir: None,
    })
    .expect("asset server");
    app.insert_resource(server);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        boxed(Vec3::new(5.0, 0.5, 5.0)),
    ));
    let body = |x: f32| {
        (
            Transform::from_translation(Vec3::new(x, 1.0, 0.0)),
            RigidBody::dynamic(),
            AuthoredCollision::new("assets/props/crate.glb"),
        )
    };
    let (left_id, right_id) = if invert_ids {
        let mut left = bevy_ecs::world::CommandQueue::default();
        let mut right = bevy_ecs::world::CommandQueue::default();
        let world = app.world_mut();
        // Recorded first: reserves the lower entity id (what a parallel system's
        // earlier-recorded queue reserves on another thread count).
        let right_id = Commands::new(&mut right, world).spawn(body(0.3)).id();
        let left_id = Commands::new(&mut left, world).spawn(body(-0.3)).id();
        // Applied in system order: the left body still arrives first.
        left.apply(world);
        right.apply(world);
        (left_id, right_id)
    } else {
        (
            app.world_mut().spawn(body(-0.3)).id(),
            app.world_mut().spawn(body(0.3)).id(),
        )
    };
    let mut digests = Vec::new();
    // CollisionStarted in delivery order: rapier emits pair events in edge-id order, and the
    // edge ids follow the collider insertion order — the order this test pins. Entities map
    // to scene roles: the ids themselves differ between the two timings by design.
    let role = |e: Entity| {
        if e == left_id {
            "left"
        } else if e == right_id {
            "right"
        } else {
            "other"
        }
    };
    let mut started = Vec::new();
    let mut cursor = MessageCursor::<CollisionStarted>::default();
    for _ in 0..120 {
        app.update();
        digests.push(
            app.world()
                .resource::<regia_physics::PhysicsWorld>()
                .state_digest(),
        );
        let messages = app.world().resource::<Messages<CollisionStarted>>();
        started.extend(
            cursor
                .read(messages)
                .map(|m| (role(m.entity_a), role(m.entity_b))),
        );
    }
    let _ = std::fs::remove_dir_all(&root);
    (digests, started)
}

/// `attach_authored_colliders` sorted its pending list by entity id, which parallel
/// `Commands::spawn` reserves in thread-timing order: the collider handles, the contact
/// graph and the settling simulation all followed the ids. The attach order must follow
/// the `AuthoredCollision` arrival order instead (ADR-1347).
#[test]
fn authored_collision_attach_order_does_not_follow_entity_ids() {
    let natural = authored_digests(false);
    let inverted = authored_digests(true);
    assert!(
        !natural.1.is_empty(),
        "the bodies must touch the floor and start contacts"
    );
    assert_eq!(
        natural.1, inverted.1,
        "collision events must follow the collider insertion (arrival) order, not entity ids"
    );
    assert_eq!(
        natural.0, inverted.0,
        "colliders from AuthoredCollision must attach in arrival order, not entity-id order"
    );
}

// ---------------------------------------------------------------------------
// Query result order (ADR-1347)
// ---------------------------------------------------------------------------

/// Two overlapping static boxes queried with `overlap` and `overlap_aabb`, entities mapped
/// to scene roles. With `invert_ids`, both ids are reserved first and the components applied
/// left-before-right (the two ADR-1347 timings): the results must follow the collider
/// insertion (arrival) order in both.
fn overlap_roles(invert_ids: bool) -> (Vec<&'static str>, Vec<&'static str>) {
    let mut app = physics_app();
    let body = |x: f32| {
        (
            Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
            RigidBody::static_body(),
            boxed(Vec3::splat(0.5)),
        )
    };
    let (left_id, right_id) = if invert_ids {
        let mut left = bevy_ecs::world::CommandQueue::default();
        let mut right = bevy_ecs::world::CommandQueue::default();
        let world = app.world_mut();
        let right_id = Commands::new(&mut right, world).spawn(body(0.3)).id();
        let left_id = Commands::new(&mut left, world).spawn(body(-0.3)).id();
        left.apply(world);
        right.apply(world);
        (left_id, right_id)
    } else {
        (
            app.world_mut().spawn(body(-0.3)).id(),
            app.world_mut().spawn(body(0.3)).id(),
        )
    };
    for _ in 0..5 {
        app.update();
    }
    let world = app.world().resource::<PhysicsWorld>();
    let queries = WorldQueries { world, layers: None };
    let role = |e: Entity| {
        if e == left_id {
            "left"
        } else if e == right_id {
            "right"
        } else {
            "other"
        }
    };
    let shape = || ColliderShape::Box {
        half_extents: Vec3::splat(2.0),
    };
    let overlap: Vec<_> = queries
        .overlap(&shape(), Transform::IDENTITY, QueryFilter::new())
        .iter()
        .map(|e| role(*e))
        .collect();
    let aabb: Vec<_> = queries
        .overlap_aabb(Vec3::splat(-2.0), Vec3::splat(2.0), QueryFilter::new())
        .iter()
        .map(|e| role(*e))
        .collect();
    (overlap, aabb)
}

/// `overlap` and `overlap_aabb` sorted by entity id, so two overlapping colliders swapped
/// places in the returned Vec between thread counts, and a lockstep script acting on
/// `hits[0]` diverged. They sort by collider handle (insertion order) now (ADR-1347).
#[test]
fn overlap_order_follows_collider_handles_not_entity_ids() {
    let natural = overlap_roles(false);
    let inverted = overlap_roles(true);
    assert_eq!(
        natural.0,
        ["left", "right"],
        "arrival order is left first: {natural:?}"
    );
    assert_eq!(natural, inverted, "query order must not follow entity ids");
}
