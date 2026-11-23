// SPDX-License-Identifier: MPL-2.0
//! Schedule-shuffle checks: run one headless scene under several seeded system orders and report any
//! run whose observable state differs (`book/agents/schedule-shuffle.md`).
//!
//! [`regia_app::App::shuffle_schedules`] re-sorts the systems that have no ordering between them.
//! A scene whose state depends on that order has a missing `.before`/`.after` edge. [`snapshot`]
//! is the observable state: every reflected component of every entity as canonical text, keyed by the
//! entity's `Name` (entity ids are not stable across orders), with entity references printed as
//! names. [`shuffle_check`] compares a snapshot per seed with the first seed's.
//!
//! [`smoke_app`] is the scene `cargo xtask shuffle-check` runs: rigid bodies, a character, spring
//! bones and a clip-driven rig with a socket, all on the CPU (no GPU, so it runs on lavapipe-only
//! machines). [`engine_app`] adds the ragdoll, script and headless render plugins to the app
//! (never run) so [`regia_app::App::ambiguity_report`] covers their schedules too.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use regia_animation::clip::{JointTrack, TrackCurve};
use regia_animation::components::spawn_skeleton;
use regia_animation::prelude::*;
use regia_animation::skeleton::LocalBind;
use regia_animation::spring::{SpringChain, SpringCollider};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState,
    use_rapier_character_controller,
};
use regia_physics::{CharacterBody, Collider, RigidBody, Velocity};
use regia_scene::Transform;
use regia_scene::components::Name;

/// Every component of every entity as `"<entity>/<Component>" -> canonical text`
/// (`regia_framework::state_hash`, where map and set entries are sorted so a `HashMap`'s
/// iteration order never shows).
pub type Snapshot = regia_framework::state_hash::StateSnapshot;

pub use regia_framework::state_hash::{differing_keys, fnv1a, snapshot_hash};

/// One frame step of the smoke scene: 16 ms, one fixed step per frame at 62.5 Hz.
const FRAME: Duration = Duration::from_millis(16);

/// The world's observable state: [`regia_framework::state_hash::world_snapshot`]. Entities are
/// keyed by `Name`; an unnamed entity by the set of its component types (and a rank among entities
/// with the same set, ordered by content), so the key does not depend on entity ids. Entity
/// references inside values print as those keys. Resources are not included.
pub fn snapshot(world: &World) -> Snapshot {
    regia_framework::state_hash::world_snapshot(world)
}

/// [`snapshot`] restricted to the components whose short type name passes `keep`
/// ([`regia_framework::state_hash::world_snapshot_where`]): for example only a game's gameplay
/// components, leaving out asset handles and render state. Entities with no kept component are
/// left out, and unnamed entities are keyed and ranked by their kept components only.
pub fn snapshot_where(world: &World, keep: impl Fn(&str) -> bool) -> Snapshot {
    regia_framework::state_hash::world_snapshot_where(world, keep)
}

/// One seed's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRun {
    /// The shuffle seed.
    pub seed: u64,
    /// Hash of the final snapshot.
    pub hash: u64,
    /// Keys (`<entity>/<Component>`) that differ from the reference run; empty when it matches.
    pub differing: Vec<String>,
}

/// What [`shuffle_check`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShuffleOutcome {
    /// Frames each run stepped.
    pub frames: u32,
    /// The seed every other run is compared with (the first seed given).
    pub reference_seed: u64,
    /// Hash of the reference run.
    pub reference_hash: u64,
    /// Entities with reflected components in the reference snapshot.
    pub entities: usize,
    /// Component values in the reference snapshot.
    pub values: usize,
    /// Whether running the reference seed a second time gave the same snapshot. When false the scene
    /// (or a plugin) is nondeterministic by itself and no seed comparison means anything.
    pub repeatable: bool,
    /// The repeat run's differing keys when `repeatable` is false.
    pub repeat_differing: Vec<String>,
    /// The other seeds.
    pub runs: Vec<SeedRun>,
}

impl ShuffleOutcome {
    /// The seeds whose state differs from the reference.
    pub fn divergent(&self) -> Vec<&SeedRun> {
        self.runs.iter().filter(|r| !r.differing.is_empty()).collect()
    }

    /// True when the scene is repeatable and no seed diverged.
    pub fn is_clean(&self) -> bool {
        self.repeatable && self.divergent().is_empty()
    }

    /// A short markdown report.
    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "shuffle-check: {} frames, {} entities, {} values; reference seed {} hash {:016x}\n",
            self.frames, self.entities, self.values, self.reference_seed, self.reference_hash
        );
        let _ = writeln!(
            out,
            "- repeat of the reference seed: {}",
            if self.repeatable {
                "identical".to_string()
            } else {
                format!("DIFFERS in {:?}", self.repeat_differing)
            }
        );
        let _ = writeln!(
            out,
            "- {} of {} other seeds diverge",
            self.divergent().len(),
            self.runs.len()
        );
        for run in self.divergent() {
            let _ = writeln!(
                out,
                "  - seed {} hash {:016x}: {}",
                run.seed,
                run.hash,
                run.differing.join(", ")
            );
        }
        out
    }
}

/// Runs `build()` once per seed (with schedules shuffled by that seed), steps `frames` frames and
/// compares each final [`snapshot`] with the first seed's. The first seed runs twice, to prove the
/// scene is repeatable before any comparison.
pub fn shuffle_check(
    build: impl Fn() -> App,
    frames: u32,
    seeds: impl IntoIterator<Item = u64>,
) -> ShuffleOutcome {
    let run = |seed: u64| {
        let mut app = build();
        app.shuffle_schedules(seed);
        for _ in 0..frames {
            app.update();
        }
        snapshot(app.world())
    };
    let mut seeds = seeds.into_iter();
    let reference_seed = seeds.next().unwrap_or(0);
    let reference = run(reference_seed);
    let repeat = run(reference_seed);
    let repeat_differing = differing_keys(&reference, &repeat, 12);
    let runs = seeds
        .map(|seed| {
            let snap = run(seed);
            SeedRun {
                seed,
                hash: snapshot_hash(&snap),
                differing: differing_keys(&reference, &snap, 12),
            }
        })
        .collect();
    ShuffleOutcome {
        frames,
        reference_seed,
        reference_hash: snapshot_hash(&reference),
        entities: reference
            .keys()
            .filter_map(|k| k.split('/').next())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        values: reference.len(),
        repeatable: repeat_differing.is_empty(),
        repeat_differing,
        runs,
    }
}

fn named(app: &mut App, name: &str, bundle: impl Bundle) -> Entity {
    app.world_mut().spawn((Name::new(name), bundle)).id()
}

fn rope() -> Skeleton {
    let names: Vec<String> = ["anchor", "c0", "c1", "c2"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let binds = (0..4)
        .map(|i| LocalBind {
            translation: if i == 0 { Vec3::ZERO } else { Vec3::X },
            ..LocalBind::default()
        })
        .collect();
    Skeleton::from_local_bind(names, vec![None, Some(0), Some(1), Some(2)], binds)
        .unwrap_or_else(|_| unreachable!("the rope skeleton is valid"))
}

/// The plugins every scene here needs: time, scene, physics, animation, the character controller
/// and the framework's gameplay helpers. All CPU-side.
fn add_core_plugins(app: &mut App) {
    app.insert_resource(TimeUpdateStrategy::ManualDuration(FRAME));
    let mut fixed = FixedTime::default();
    fixed.hz = 62.5;
    app.insert_resource(fixed);
    app.add_plugins((
        MinimalPlugins,
        regia_scene::ScenePlugin,
        regia_physics::PhysicsPlugin,
        AnimationPlugin,
        regia_framework::skinning::SkinnedMeshPlugin,
        CharacterPlugin,
    ));
    app.add_plugins((
        regia_framework::tween::TweenPlugin,
        regia_framework::timers::TimerPlugin,
        regia_framework::damage::DamagePlugin,
        regia_framework::coroutine::CoroutinePlugin,
    ));
    use_rapier_character_controller(app);
}

/// The headless scene `cargo xtask shuffle-check` runs, with deterministic time.
///
/// A ground box; eight dynamic boxes dropped in a row so they collide; a character walking across
/// the ground; a rope with spring bones hanging off a collider; and a rig whose clip lifts one joint
/// with a socket entity on it.
pub fn smoke_app() -> App {
    let mut app = App::new();
    add_core_plugins(&mut app);
    let half = Vec3::new(30.0, 0.5, 30.0);
    named(
        &mut app,
        "ground",
        (
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: regia_physics::ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ),
    );
    for i in 0..8 {
        let x = i as f32 * 0.55;
        named(
            &mut app,
            &format!("box{i}"),
            (
                Transform::from_translation(Vec3::new(x, 1.0 + i as f32 * 0.3, 0.1 * i as f32)),
                RigidBody::dynamic(),
                Collider {
                    shape: regia_physics::ColliderShape::Box {
                        half_extents: Vec3::splat(0.25),
                    },
                    ..Collider::default()
                },
                Velocity::default(),
            ),
        );
    }
    named(
        &mut app,
        "hero",
        (
            Transform::from_translation(Vec3::new(-3.0, 1.3, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput {
                move_dir: Vec3::X,
                ..CharacterInput::default()
            },
            CharacterState::default(),
            CharacterBody::default(),
        ),
    );
    // A rope with spring bones.
    let rig = named(
        &mut app,
        "rope_rig",
        Transform::from_translation(Vec3::new(0.0, 3.0, 4.0)),
    );
    let binding = spawn_skeleton(app.world_mut(), Arc::new(rope()));
    app.world_mut().entity_mut(rig).insert((
        binding,
        Animator::single(regia_animation::AnimationClip::empty("hold", 1.0), true),
        SpringBones::new(SpringBoneSetup {
            chains: vec![SpringChain {
                name: "rope".into(),
                joints: vec!["c0".into(), "c1".into(), "c2".into()],
                stiffness: 0.0,
                damping: 0.15,
                gravity_power: 9.81,
                hit_radius: 0.02,
                colliders: vec!["post".into()],
                ..SpringChain::default()
            }],
            colliders: vec![SpringCollider {
                name: "post".into(),
                joint: "anchor".into(),
                offset: Vec3::new(1.0, -1.4, 0.0),
                shape: regia_animation::ColliderShape::Capsule {
                    radius: 0.2,
                    tail: Vec3::new(1.0, -3.0, 0.0),
                },
            }],
            ..SpringBoneSetup::default()
        }),
    ));
    // A clip-driven arm with a socket.
    let arm_skeleton = Arc::new(
        Skeleton::from_local_bind(
            vec!["root".into(), "arm".into()],
            vec![None, Some(0)],
            vec![
                LocalBind::default(),
                LocalBind {
                    translation: Vec3::Y,
                    ..LocalBind::default()
                },
            ],
        )
        .unwrap_or_else(|_| unreachable!("the arm skeleton is valid")),
    );
    let mut clip = regia_animation::AnimationClip::empty("lift", 1.0);
    clip.tracks.push(JointTrack {
        joint: 1,
        translations: TrackCurve::Animated {
            times: vec![0.0, 1.0],
            values: vec![Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 3.0, 0.0)],
            exact_rotation: false,
        },
        rotations: TrackCurve::Static(Quat::IDENTITY),
        scales: TrackCurve::Static(Vec3::ONE),
    });
    let arm = named(
        &mut app,
        "arm_rig",
        Transform::from_translation(Vec3::new(4.0, 0.0, 0.0)),
    );
    let binding = spawn_skeleton(app.world_mut(), arm_skeleton);
    app.world_mut()
        .entity_mut(arm)
        .insert((binding, Animator::single(clip, true)));
    named(
        &mut app,
        "hand",
        (
            Transform::default(),
            Socket {
                rig: arm,
                name: "hand".into(),
                joint: "arm".into(),
                offset: Transform::default(),
            },
        ),
    );
    app
}

/// [`smoke_app`] plus the engine's other plugins, for [`regia_app::App::ambiguity_report`]: the
/// headless render plugin (extraction and render schedules), ragdolls and scripts. UI and audio
/// are not added. Build it and ask for the report; do not run it (the render schedule wants a
/// device).
pub fn engine_app() -> App {
    let mut app = smoke_app();
    app.add_plugins((
        regia_framework::ragdoll::RagdollPlugin,
        regia_framework::script::ScriptPlugin,
        regia_render::RenderPlugin { headless: true },
    ));
    app
}
