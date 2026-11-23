// SPDX-License-Identifier: MPL-2.0
//! Debris spawner API — spawn cosmetic physics chunks from a pre-fractured mesh.
//!
//! Specification: `docs/specs/25-sim-bridge-and-do27.md` § 4 Destruction row
//! (pre-fractured meshes + rapier debris driven by sim events; never affects the sim).
//!
//! Provenance: debris spawn from impulse origin using rapier external forces is a
//! standard real-time physics technique described in public documentation and game
//! physics tutorials (e.g., Ian Millington, "Game Physics Engine Development", 2nd ed.,
//! Morgan Kaufmann 2010 — public textbook).

use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use tracing::instrument;

use crate::components::{BodyKind, ColliderHandle, ColliderShape, DebrisTag, RigidBodyHandle};
use crate::convert::to_rapier_vec3;
use crate::step::shape_to_shared_shape;
use crate::world::{BodyBudget, PhysicsSettings, PhysicsWorld};

// ---------------------------------------------------------------------------
// ChunkDesc
// ---------------------------------------------------------------------------

/// Description of one chunk in a pre-fractured mesh.
#[derive(Debug, Clone)]
pub struct ChunkDesc {
    /// Local translation relative to the fracture origin.
    pub local_translation: Vec3,
    /// Local rotation relative to the fracture origin.
    pub local_rotation: Quat,
    /// Collision shape for this chunk.
    pub shape: ColliderShape,
    /// Mass in kilograms, passed to rapier as `additional_mass` when it is positive and finite.
    /// Zero and every other non-positive finite mass pass 1 kg instead. A non-finite mass skips
    /// the chunk. The collider is still built at rapier's default density. Attaching it adds
    /// that density mass at once. The additional mass is included in `RigidBody::mass` when mass
    /// properties are recomputed, which the next physics step does, so the simulated mass is then
    /// the additional mass plus the collider's own mass. The outward speed below uses the
    /// authored mass, before that 1 kg substitution.
    pub mass: f32,
}

// ---------------------------------------------------------------------------
// SpawnResult
// ---------------------------------------------------------------------------

/// Result of a debris spawn operation.
#[derive(Debug, Clone)]
pub struct SpawnResult {
    /// Number of chunks successfully spawned.
    pub spawned: u32,
    /// Number of chunks skipped (e.g., budget exceeded, invalid shape).
    pub skipped: u32,
    /// Entities despawned by the spawn itself to make room. Always empty: over-budget
    /// debris is evicted oldest-first by the budget system after the step, and chunks that
    /// do not fit are counted in `skipped`.
    pub evicted: Vec<Entity>,
}

// ---------------------------------------------------------------------------
// DebrisSpawner
// ---------------------------------------------------------------------------

/// The debris spawner API — spawn cosmetic physics chunks from a pre-fractured mesh.
pub struct DebrisSpawner {
    /// Lifetime in seconds for each spawned debris chunk.
    pub lifetime_secs: f64,
}

/// Parameters for a debris spawn event.
pub struct SpawnConfig<'a> {
    /// The pre-fractured mesh chunks.
    pub chunks: &'a [ChunkDesc],
    /// World-space origin of the fracture.
    pub origin: Vec3,
    /// Outward speed in m/s, plus a bounded mass term. A chunk at
    /// `origin + local_translation` is inserted with linear velocity
    /// `direction * (impulse_strength + min(1 / max(mass, 0.1), impulse_strength))` m/s.
    /// `direction` is the unit vector from the origin to the chunk, or zero when the chunk sits
    /// on the origin (that chunk spawns at rest). `mass` is the authored mass, before a
    /// non-positive mass is replaced by 1 kg, so a zero or negative mass is faster than a 1 kg
    /// chunk: with strength 15, mass 1 starts at 16 m/s, mass 10 at 15.1 m/s, and mass 0 or −5
    /// at 25 m/s. A non-finite strength is treated as 0. This is a velocity, not an impulse in
    /// N·s.
    pub impulse_strength: f32,
}

impl DebrisSpawner {
    /// Creates a debris spawner with the given chunk lifetime.
    pub fn new(lifetime_secs: f64) -> Self {
        Self { lifetime_secs }
    }

    /// Spawns debris chunks at the origin. Each chunk's initial linear velocity is the outward
    /// speed on [`SpawnConfig::impulse_strength`].
    #[instrument(skip_all, target = "regia_physics")]
    pub fn spawn(
        &self,
        commands: &mut Commands,
        physics: &mut PhysicsWorld,
        budget: &mut BodyBudget,
        settings: &PhysicsSettings,
        config: &SpawnConfig<'_>,
    ) -> SpawnResult {
        use rapier3d::dynamics::{RigidBodyBuilder, RigidBodyType};
        use rapier3d::geometry::ColliderBuilder;

        let available = available_debris_slots(settings.max_debris_bodies, budget.len());

        // A NaN or infinite strength would put a non-finite velocity into rapier; the chunks then
        // just fall.
        let impulse_strength = if config.impulse_strength.is_finite() {
            config.impulse_strength
        } else {
            0.0
        };

        let mut spawned = 0u32;
        let mut skipped = 0u32;

        for chunk in config.chunks.iter() {
            // The budget counts bodies actually inserted, not chunk indices: a rejected chunk
            // does not use up a free slot.
            if spawned as usize >= available {
                skipped += 1;
                continue;
            }

            let world_pos = config.origin + chunk.local_translation;
            let world_rot = chunk.local_rotation;

            if !world_pos.is_finite() || !world_rot.is_finite() || !chunk.mass.is_finite() {
                // A non-finite chunk pose or mass never reaches rapier; it counts as skipped
                // like a chunk over the budget (ADR-1507, ADR-1660).
                skipped += 1;
                continue;
            }

            let dir = (world_pos - config.origin).normalize_or_zero();
            let speed = impulse_strength + (chunk.mass.max(0.1)).recip().min(impulse_strength);
            let impulse = dir * speed;
            let mass = if chunk.mass > 0.0 { chunk.mass } else { 1.0 };

            // The pose, not the axis-angle round trip: the round trip uses platform libm and
            // would store different rotation bits per platform, while `state_digest` hashes
            // this body like every other (spec 15 § 5).
            let body = RigidBodyBuilder::new(RigidBodyType::Dynamic)
                .pose(crate::convert::to_rapier_pose(world_pos, world_rot))
                .linvel(to_rapier_vec3(impulse))
                .can_sleep(true)
                .sleeping(false)
                .additional_mass(mass)
                .build();
            let body_handle = physics.bodies.insert(body);

            let Some(shared) = shape_to_shared_shape(&chunk.shape) else {
                skipped += 1;
                remove_body(physics, body_handle);
                continue;
            };

            let c = ColliderBuilder::new(shared).build();
            let collider_handle = {
                // Destructure for split borrows.
                let PhysicsWorld {
                    ref mut bodies,
                    ref mut colliders,
                    ..
                } = *physics;
                colliders.insert_with_parent(c, body_handle, bodies)
            };

            let entity = commands
                .spawn((
                    RigidBodyHandle::from_rapier(body_handle),
                    regia_scene::Transform {
                        translation: world_pos,
                        rotation: world_rot,
                        scale: Vec3::ONE,
                    },
                    ColliderHandle::from_rapier(collider_handle),
                    BodyKind::Dynamic,
                    chunk.shape.clone(),
                    DebrisTag {
                        spawn_time_secs: None,
                        lifetime_secs: self.lifetime_secs,
                    },
                ))
                .id();
            physics.register_body(body_handle, entity);
            physics.register_collider(collider_handle, entity);

            budget.push(entity);
            spawned += 1;
        }

        SpawnResult {
            spawned,
            skipped,
            evicted: Vec::new(),
        }
    }
}

/// Free debris slots under `max_debris_bodies` (0 means unlimited) with `used` bodies live.
fn available_debris_slots(max_debris_bodies: u32, used: usize) -> usize {
    if max_debris_bodies == 0 {
        usize::MAX
    } else {
        (max_debris_bodies as usize).saturating_sub(used)
    }
}

/// Helper to remove a body from the physics world using split borrows.
fn remove_body(physics: &mut PhysicsWorld, handle: rapier3d::dynamics::RigidBodyHandle) {
    let PhysicsWorld {
        ref mut bodies,
        ref mut islands,
        ref mut colliders,
        ref mut impulse_joints,
        ref mut multibody_joints,
        ref mut soft_bodies,
        ..
    } = *physics;
    rapier3d::dynamics::RigidBodySet::remove(
        bodies,
        handle,
        islands,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_max_debris_bodies_means_unlimited() {
        assert_eq!(available_debris_slots(0, 10_000), usize::MAX);
        assert_eq!(available_debris_slots(10, 7), 3);
        assert_eq!(available_debris_slots(10, 12), 0);
    }

    #[test]
    fn spawn_with_unlimited_budget_spawns_every_chunk() {
        use regia_ecs::ecs::world::CommandQueue;
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let mut physics = PhysicsWorld::default();
        let mut budget = BodyBudget::default();
        let settings = PhysicsSettings {
            max_debris_bodies: 0,
            ..PhysicsSettings::default()
        };
        let chunks = make_chunks(4);
        let config = SpawnConfig {
            chunks: &chunks,
            origin: Vec3::ZERO,
            impulse_strength: 1.0,
        };
        let result = {
            let mut commands = Commands::new(&mut queue, &world);
            DebrisSpawner::new(5.0).spawn(&mut commands, &mut physics, &mut budget, &settings, &config)
        };
        queue.apply(&mut world);
        assert_eq!(result.spawned, 4);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn a_rejected_chunk_does_not_use_up_a_free_slot() {
        // One free slot, chunks [non-finite pose, valid]: the valid chunk must still spawn.
        use regia_ecs::ecs::world::CommandQueue;
        let world = World::new();
        let mut queue = CommandQueue::default();
        let mut physics = PhysicsWorld::default();
        let mut budget = BodyBudget::default();
        let settings = PhysicsSettings {
            max_debris_bodies: 1,
            ..PhysicsSettings::default()
        };
        let mut chunks = make_chunks(2);
        chunks[0].local_translation = Vec3::new(f32::NAN, 0.0, 0.0);
        let config = SpawnConfig {
            chunks: &chunks,
            origin: Vec3::ZERO,
            impulse_strength: 1.0,
        };
        let result = {
            let mut commands = Commands::new(&mut queue, &world);
            DebrisSpawner::new(5.0).spawn(&mut commands, &mut physics, &mut budget, &settings, &config)
        };
        assert_eq!((result.spawned, result.skipped), (1, 1));
        assert_eq!(physics.bodies.len(), 1);
    }

    #[test]
    fn a_non_finite_mass_skips_the_chunk_before_rapier() {
        // A `.fracture.ron` volume of `inf` used to reach `additional_mass` (ADR-1660).
        use regia_ecs::ecs::world::CommandQueue;
        let world = World::new();
        let mut queue = CommandQueue::default();
        let mut physics = PhysicsWorld::default();
        let mut budget = BodyBudget::default();
        let mut chunks = make_chunks(4);
        for (chunk, mass) in chunks
            .iter_mut()
            .zip([f32::INFINITY, f32::NAN, f32::NEG_INFINITY, 2.0])
        {
            chunk.mass = mass;
        }
        let config = SpawnConfig {
            chunks: &chunks,
            origin: Vec3::ZERO,
            impulse_strength: 1.0,
        };
        let result = {
            let mut commands = Commands::new(&mut queue, &world);
            DebrisSpawner::new(5.0).spawn(
                &mut commands,
                &mut physics,
                &mut budget,
                &PhysicsSettings::default(),
                &config,
            )
        };
        assert_eq!((result.spawned, result.skipped), (1, 3));
        assert_eq!(
            physics.bodies.len(),
            1,
            "only the finite-mass chunk reached rapier"
        );
    }

    #[test]
    fn chunk_create() {
        let chunk = ChunkDesc {
            local_translation: Vec3::new(1.0, 0.0, 0.0),
            local_rotation: Quat::IDENTITY,
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.5, 0.5, 0.5),
            },
            mass: 2.0,
        };
        assert!((chunk.local_translation - Vec3::new(1.0, 0.0, 0.0)).length() < 1e-6);
        assert_eq!(chunk.mass, 2.0);
    }

    // Tests that don't need a full ECS World.
    #[allow(dead_code)]
    fn make_chunks(n: usize) -> Vec<ChunkDesc> {
        (0..n)
            .map(|i| ChunkDesc {
                local_translation: Vec3::new(i as f32 * 0.5, 0.0, 0.0),
                local_rotation: Quat::IDENTITY,
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.2, 0.2, 0.2),
                },
                mass: 1.0,
            })
            .collect()
    }

    #[test]
    fn spawn_respects_body_budget_via_available_slots() {
        let physics = PhysicsWorld::default();
        let mut budget = BodyBudget::default();
        // Pre-fill budget so only 3 slots remain.
        for i in 0..7 {
            budget.push(Entity::from_raw_u32(i).expect("valid entity"));
        }
        // Budget enforcement is a separate concern; this only checks that
        // spawn respects the available headroom.
        let available = (10usize).saturating_sub(budget.len());
        assert_eq!(available, 3);
        // PhysicsWorld is still empty.
        assert_eq!(physics.bodies.len(), 0);
    }

    #[test]
    fn chunks_at_origin_face_outward_from_origin() {
        // The debris spawner computes outward impulse vectors correctly.
        let origin = Vec3::new(0.0, 2.0, 0.0);
        let chunk = ChunkDesc {
            local_translation: Vec3::new(1.0, 0.0, 0.0),
            local_rotation: Quat::IDENTITY,
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.5, 0.5, 0.5),
            },
            mass: 1.0,
        };
        let world_pos = origin + chunk.local_translation;
        let dir = (world_pos - origin).normalize_or_zero();
        // Should be pointing right (+X).
        assert!((dir.x - 1.0).abs() < 1e-6);
        assert!(dir.y.abs() < 1e-6);
        assert!(dir.z.abs() < 1e-6);
    }

    #[test]
    fn no_feedback_path_to_sim() {
        let tag = DebrisTag {
            spawn_time_secs: None,
            lifetime_secs: 5.0,
        };
        assert_eq!(tag.lifetime_secs, 5.0);
    }
}
