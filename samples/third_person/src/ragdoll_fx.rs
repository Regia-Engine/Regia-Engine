// SPDX-License-Identifier: MIT-0
//! Death ragdolls (ADR-2596): a fallen fighter is a limp `regia_framework::ragdoll::Ragdoll` on a
//! small procedural humanoid skeleton, so a death falls the same way with or without the Fox
//! (the Fox is a quadruped, and headless tests may run without its download). The ragdoll's
//! bodies get box meshes when they are built, and a `RagdollHit` along the killing blow's
//! direction knocks the body over.
//!
//! Corpses beyond [`MAX_CORPSES`] are removed oldest first ([`regia_framework::ragdoll::remove_ragdoll`]).

use std::sync::Arc;

use regia::prelude::*;
use regia_animation::components::spawn_skeleton;
use regia_animation::pose::translated;
use regia_animation::{AnimationClip, Animator, Skeleton};
use regia_core::math::Vec3;
use regia_framework::ragdoll::{Ragdoll, RagdollBodies, RagdollHit, RagdollMode, RagdollProfile};
use regia_physics::{Collider, ColliderShape, ShapeAxis};
use regia_render::MeshRenderer;
use regia_scene::Transform;

/// Most corpses kept in the level.
pub const MAX_CORPSES: usize = 6;

/// Joint names of the proxy humanoid (the names `RetargetProfile::guess_humanoid` recognises).
const NAMES: [&str; 17] = [
    "hips",
    "spine",
    "chest",
    "neck",
    "head",
    "upperarm_l",
    "lowerarm_l",
    "hand_l",
    "upperarm_r",
    "lowerarm_r",
    "hand_r",
    "thigh_l",
    "calf_l",
    "foot_l",
    "thigh_r",
    "calf_r",
    "foot_r",
];

/// The proxy humanoid: hips 0.95 m above the rig origin (the feet), 1.75 m tall.
pub fn proxy_skeleton() -> Option<Skeleton> {
    let parents = vec![
        None,
        Some(0),
        Some(1),
        Some(2),
        Some(3),
        Some(2),
        Some(5),
        Some(6),
        Some(2),
        Some(8),
        Some(9),
        Some(0),
        Some(11),
        Some(12),
        Some(0),
        Some(14),
        Some(15),
    ];
    let t = |x: f32, y: f32| translated(Vec3::new(x, y, 0.0));
    let binds = vec![
        t(0.0, 0.95),
        t(0.0, 0.14),
        t(0.0, 0.24),
        t(0.0, 0.22),
        t(0.0, 0.1),
        t(0.19, 0.18),
        t(0.28, 0.0),
        t(0.25, 0.0),
        t(-0.19, 0.18),
        t(-0.28, 0.0),
        t(-0.25, 0.0),
        t(0.1, -0.05),
        t(0.0, -0.43),
        t(0.0, -0.4),
        t(-0.1, -0.05),
        t(0.0, -0.43),
        t(0.0, -0.4),
    ];
    Skeleton::from_local_bind(NAMES.iter().map(|n| (*n).to_string()).collect(), parents, binds).ok()
}

/// The shared proxy skeleton (built once).
#[derive(Resource, Clone)]
pub struct ProxySkeleton(pub Arc<Skeleton>);

/// A corpse rig: who it was and how it is doing (reflected for agent checks).
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct Corpse {
    /// The fallen fighter's team (`combat::TEAM_PLAYER` or `combat::TEAM_ENEMY`).
    pub team: u8,
    /// Ragdoll bodies built (0 until the first animated frame).
    pub bodies: u32,
    /// Whether the ragdoll is simulating limp.
    pub limp: bool,
    /// Whether it came to rest.
    pub settled: bool,
    /// Spawn order (oldest corpses are removed first).
    pub serial: u32,
    /// Body color (RGBA).
    pub color: [f32; 4],
    /// The knock-down impulse still to apply once the bodies exist.
    pub push: Vec3,
    /// Whether the knock-down has been applied.
    pub pushed: bool,
}

/// Next corpse serial.
#[derive(Resource, Default)]
struct CorpseSerial(u32);

/// Installs the ragdoll plugin and the corpse systems.
pub struct RagdollFxPlugin;

impl Plugin for RagdollFxPlugin {
    fn build(&self, app: &mut App) {
        if !app.has_plugin(std::any::type_name::<regia_framework::ragdoll::RagdollPlugin>()) {
            app.add_plugins(regia_framework::ragdoll::RagdollPlugin);
        }
        app.register_type::<Corpse>().init_resource::<CorpseSerial>();
        if let Some(skeleton) = proxy_skeleton() {
            app.insert_resource(ProxySkeleton(Arc::new(skeleton)));
        } else {
            tracing::error!(target: "third_person", "proxy ragdoll skeleton is invalid");
        }
        app.add_systems(
            Update,
            dress_corpses.after(regia_animation::systems::evaluate_animators),
        );
    }
}

/// Spawns a limp ragdoll where a fighter fell. `at` is the fighter's capsule transform (center
/// 0.8 m above the ground); `push` the direction of the killing blow (zero for none).
pub fn spawn_ragdoll(
    world: &mut World,
    at: Transform,
    push: Vec3,
    color: [f32; 4],
    team: u8,
) -> Option<Entity> {
    let skeleton = world.get_resource::<ProxySkeleton>()?.0.clone();
    let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 70.0));
    ragdoll.mode = RagdollMode::Limp;
    let binding = spawn_skeleton(world, skeleton);
    let serial = {
        let mut next = world.resource_mut::<CorpseSerial>();
        next.0 += 1;
        next.0
    };
    let feet = at.translation - Vec3::new(0.0, 0.8, 0.0);
    let rig = world
        .spawn((
            Transform {
                translation: feet,
                rotation: at.rotation,
                ..Default::default()
            },
            binding,
            Animator::single(AnimationClip::empty("stand", 1.0), true),
            ragdoll,
            Corpse {
                team,
                serial,
                color,
                push,
                limp: true,
                ..Default::default()
            },
            Name::new(format!("corpse-{serial}")),
        ))
        .id();
    trim_corpses(world);
    Some(rig)
}

/// Removes the oldest corpses beyond [`MAX_CORPSES`].
fn trim_corpses(world: &mut World) {
    let mut corpses: Vec<(u32, Entity)> = world
        .query::<(Entity, &Corpse)>()
        .iter(world)
        .map(|(e, c)| (c.serial, e))
        .collect();
    if corpses.len() <= MAX_CORPSES {
        return;
    }
    corpses.sort_unstable();
    let excess = corpses.len() - MAX_CORPSES;
    for (_, rig) in corpses.into_iter().take(excess) {
        let joints: Vec<Entity> = world
            .get::<regia_animation::SkinnedMesh>(rig)
            .map(|s| s.joints.clone())
            .unwrap_or_default();
        regia_framework::ragdoll::remove_ragdoll(world, rig);
        for joint in joints {
            let _ = world.try_despawn(joint);
        }
        let _ = world.try_despawn(rig);
    }
}

/// `Update`: once a corpse's bodies exist, give each a box mesh and knock it over; mirror the
/// ragdoll state into [`Corpse`].
#[tracing::instrument(skip_all, target = "third_person", name = "dress_corpses")]
pub fn dress_corpses(world: &mut World) {
    let corpses: Vec<(Entity, Corpse, Vec<Entity>, bool)> = world
        .query::<(Entity, &Corpse, &RagdollBodies)>()
        .iter(world)
        .map(|(e, c, b)| {
            (
                e,
                c.clone(),
                b.bodies.iter().map(|b| b.entity).collect(),
                b.settled(),
            )
        })
        .collect();
    for (rig, corpse, bodies, settled) in corpses {
        if !corpse.pushed {
            for &body in &bodies {
                let Some(shape) = world.get::<Collider>(body).map(|c| c.shape.clone()) else {
                    continue;
                };
                let half = match shape {
                    ColliderShape::Capsule {
                        half_height,
                        radius,
                        axis,
                    } => match axis {
                        ShapeAxis::X => Vec3::new(half_height + radius, radius, radius),
                        ShapeAxis::Y => Vec3::new(radius, half_height + radius, radius),
                        ShapeAxis::Z => Vec3::new(radius, radius, half_height + radius),
                    },
                    ColliderShape::Sphere { radius } => Vec3::splat(radius),
                    ColliderShape::Box { half_extents } => half_extents,
                    _ => Vec3::splat(0.08),
                };
                world.entity_mut(body).insert(MeshRenderer::new(
                    crate::level::box_mesh(half),
                    crate::level::solid_material(corpse.color),
                ));
            }
            let impulse = if corpse.push.length_squared() > 0.0 {
                corpse.push * 45.0 + Vec3::new(0.0, 8.0, 0.0)
            } else {
                Vec3::new(0.0, 0.0, 20.0)
            };
            world.write_message(RagdollHit {
                rig,
                joint: "chest".into(),
                impulse,
                reaction: 1.0,
                recover: 0.5,
            });
        }
        if let Some(mut c) = world.get_mut::<Corpse>(rig) {
            c.pushed = true;
            c.bodies = bodies.len() as u32;
            c.settled = settled;
        }
    }
}
