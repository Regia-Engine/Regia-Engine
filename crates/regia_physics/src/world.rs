// SPDX-License-Identifier: MPL-2.0
//! Physics world resource and settings.
//!
//! Holds rapier's sets, the physics pipeline, integration parameters, gravity, the body budget,
//! debris-despawn tracking, the collider↔entity maps used by events and queries, and the
//! step's event buffer (specs/15 § 1, § 3).

use std::collections::{HashMap, HashSet, VecDeque};

use rapier3d::dynamics::{
    CCDSolver, ImpulseJointHandle, ImpulseJointSet, IslandManager, MultibodyJointSet, RigidBodyHandle,
    RigidBodySet, SoftBodySet,
};
use rapier3d::geometry::{BroadPhaseBvh, ColliderHandle, ColliderSet, NarrowPhase};
use rapier3d::math::Vector;
use rapier3d::pipeline::{PhysicsPipeline, QueryFilter, QueryPipeline, QueryPipelineMut};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;

use crate::events::EventBuffer;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Largest constraint-solver iteration count one step uses; larger settings are clamped.
pub const MAX_SOLVER_ITERATIONS: usize = 64;

/// Largest CCD substep count one step uses; larger settings are clamped.
pub const MAX_CCD_SUBSTEPS: usize = 16;

/// Gravity and other physics parameters, set at startup.
///
/// Gravity here is the fallback used when no `LevelSettings` resource exists; the level's
/// gravity wins while a level is loaded (specs/15 § 1). The fixed-step rate is
/// [`FixedTime::hz`](regia_app::FixedTime::hz) (the schedule that runs the step);
/// `fixed_hz` is kept for the M14 debris API and ignored by the scheduled step.
#[derive(Resource, Debug, Clone)]
pub struct PhysicsSettings {
    /// Gravity vector in world space (m/s²). Default: (0, -9.81, 0).
    pub gravity: Vec3,
    /// Maximum number of cosmetic debris bodies. When exceeded, oldest debris
    /// is despawned first. Use 0 for unlimited.
    pub max_debris_bodies: u32,
    /// Fixed-step rate in Hz. Default 60. (M14 debris API; the scheduled step uses
    /// `FixedTime::hz`.)
    pub fixed_hz: f64,
    /// Constraint-solver iterations per step (rapier `num_solver_iterations`). Default 4.
    /// Raise it for tall stacks, long joint chains and large mass ratios; each iteration
    /// costs solver time. Values below 1 are treated as 1, values above
    /// [`MAX_SOLVER_ITERATIONS`] are clamped.
    pub solver_iterations: usize,
    /// Maximum CCD substeps per step (rapier `max_ccd_substeps`). Default 1. More substeps
    /// let a fast CCD body keep moving after its first impact within the same step instead
    /// of losing the rest of that step's motion. Values below 1 are treated as 1, values above
    /// [`MAX_CCD_SUBSTEPS`] are clamped.
    pub ccd_substeps: usize,
}

impl Default for PhysicsSettings {
    fn default() -> Self {
        Self {
            gravity: Vec3::new(0.0, -9.81, 0.0),
            max_debris_bodies: 512,
            fixed_hz: 60.0,
            solver_iterations: 4,
            ccd_substeps: 1,
        }
    }
}

/// Gravity extracted from [`PhysicsSettings`] for convenience.
#[derive(Resource, Debug, Clone)]
pub struct Gravity(pub Vector);

impl Default for Gravity {
    fn default() -> Self {
        Self(Vector::new(0.0, -9.81, 0.0))
    }
}

// ---------------------------------------------------------------------------
// Body budget
// ---------------------------------------------------------------------------

/// Tracks debris body count and spawn order for budget enforcement.
///
/// When the debris count exceeds [`PhysicsSettings::max_debris_bodies`], the oldest
/// debris entities are despawned (first-in-first-out).
#[derive(Resource, Debug, Default)]
pub struct BodyBudget {
    /// Debris entities in spawn order (oldest first).
    debris_order: VecDeque<Entity>,
}

impl BodyBudget {
    /// Records a new debris entity in the budget.
    pub fn push(&mut self, entity: Entity) {
        self.debris_order.push_back(entity);
    }

    /// Removes and returns the oldest debris entities exceeding `max_count`.
    /// Returns entities that should be despawned.
    pub fn enforce(&mut self, max_count: u32) -> Vec<Entity> {
        let max = max_count as usize;
        let mut overflow = Vec::new();
        while self.debris_order.len() > max {
            if let Some(oldest) = self.debris_order.pop_front() {
                overflow.push(oldest);
            }
        }
        overflow
    }

    /// Removes a specific entity from tracking (e.g., on despawn or lifetime expiry).
    pub fn remove(&mut self, entity: Entity) {
        self.debris_order.retain(|e| *e != entity);
    }

    /// Current number of tracked debris entities.
    pub fn len(&self) -> usize {
        self.debris_order.len()
    }

    /// Returns true if there are no debris entities.
    pub fn is_empty(&self) -> bool {
        self.debris_order.is_empty()
    }
}

// ---------------------------------------------------------------------------
// PhysicsWorld
// ---------------------------------------------------------------------------

/// The rapier physics world, stepped in `FixedUpdate`.
#[derive(Resource)]
pub struct PhysicsWorld {
    /// Rapier's rigid body set.
    pub bodies: RigidBodySet,
    /// Rapier's collider set.
    pub colliders: ColliderSet,
    /// Joints (empty for M14 debris).
    pub impulse_joints: ImpulseJointSet,
    /// Multibody joints (empty for M14 debris).
    pub multibody_joints: MultibodyJointSet,
    /// Soft body set (empty for M14 debris).
    pub soft_bodies: SoftBodySet,
    /// Island manager for sleeping/active set management.
    pub islands: IslandManager,
    /// Broad-phase for collision detection.
    pub broad_phase: BroadPhaseBvh,
    /// Narrow-phase for contact generation.
    pub narrow_phase: NarrowPhase,
    /// CCD solver.
    pub ccd_solver: CCDSolver,
    /// The physics pipeline.
    pub pipeline: PhysicsPipeline,
    /// Accumulated time since last step, in seconds (used by manual stepping; the
    /// scheduled step runs once per `FixedUpdate`).
    pub accumulator: f64,
    /// Collider handle → owning ECS entity (events, queries).
    pub(crate) collider_entities: HashMap<ColliderHandle, Entity>,
    /// Rigid-body handle → owning ECS entity (removal, joints).
    pub(crate) body_entities: HashMap<RigidBodyHandle, Entity>,
    /// ECS entity → rigid-body handle (removal bookkeeping).
    pub(crate) body_handles: HashMap<Entity, RigidBodyHandle>,
    /// ECS entity → collider handle (removal bookkeeping).
    pub(crate) collider_handles: HashMap<Entity, ColliderHandle>,
    /// Joint-carrying entity → joint handle (removal, break-force bookkeeping).
    pub(crate) joint_handles: HashMap<Entity, ImpulseJointHandle>,
    /// Collision events buffered during the last step, drained into ECS messages.
    pub(crate) event_buffer: EventBuffer,
    /// Colliders removed since the last step: handle → (owning entity, was a sensor). The
    /// next step reports their `Stopped` pairs, and the event flush still needs the entity.
    pub(crate) removed_awaiting_step: HashMap<ColliderHandle, (Entity, bool)>,
    /// Colliders removed before the step that just ran; cleared by the event flush.
    pub(crate) removed_stepped: HashMap<ColliderHandle, (Entity, bool)>,
    /// Joint entities whose joint broke (`break_force`): not re-created until the `Joint`
    /// component changes.
    pub(crate) broken_joints: HashSet<Entity>,
    /// Steps run by [`crate::step_physics`] since the world was created.
    pub(crate) step_count: u64,
    /// Sum of the fixed step lengths of those steps: the clock debris lifetimes run on, so a
    /// chunk leaves the solver on the same step at any frame rate (ADR-1660).
    pub(crate) fixed_elapsed: std::time::Duration,
    /// Wall time of the last `PhysicsPipeline::step` call, in seconds (diagnostics only).
    pub(crate) last_step_secs: f64,
    /// Non-finite body poses, body fields, velocities, forces and impulses dropped at the
    /// rapier seam since the world was created (ADR-1507): each would otherwise spread NaN
    /// through the solver. Surfaced as [`PhysicsStats::non_finite_inputs`].
    pub(crate) non_finite_inputs: u64,
    /// Removals recorded by the plugin's observers, drained by [`crate::sync_removed`].
    pub(crate) pending_removals: PendingRemovals,
}

/// Component removals and despawns recorded the moment they happen (by `Remove` observers
/// that [`crate::PhysicsPlugin`] registers) and applied by [`crate::sync_removed`] before the
/// next step.
///
/// `RemovedComponents` would lose them: the app clears removal messages every frame, and
/// when the render rate is more than twice the fixed rate (144 Hz display, 60 Hz physics)
/// two frames can pass without a fixed step, so a despawned body would stay in the rapier
/// world as a ghost. Handle removals carry the handle, so a body that was removed and
/// re-created in between is never taken down by a stale record.
#[derive(Debug, Default)]
pub(crate) struct PendingRemovals {
    /// `RigidBodyHandle` left an entity (despawn, or physics removed it), with the handle.
    pub(crate) body_handles: Vec<(Entity, RigidBodyHandle)>,
    /// `ColliderHandle` left an entity, with the handle.
    pub(crate) collider_handles: Vec<(Entity, ColliderHandle)>,
    /// The user removed `RigidBody` (also fires on despawn).
    pub(crate) bodies: Vec<Entity>,
    /// The user removed `Collider` (also fires on despawn).
    pub(crate) colliders: Vec<Entity>,
    /// The user removed `Joint` (also fires on despawn).
    pub(crate) joints: Vec<Entity>,
    /// The user removed `ExternalForce`: its rapier force must be reset.
    pub(crate) forces: Vec<Entity>,
    /// The user removed `JointNoContacts` (also fires on despawn): the joint's contacts are
    /// enabled again. Drained by `sync_joints`, not by `sync_removed` (ADR-2396).
    pub(crate) joint_no_contacts: Vec<Entity>,
}

/// Counts that describe a [`PhysicsWorld`] (agent `physics.stats`, perf triage).
///
/// "Awake" and "sleeping" count dynamic bodies only: rapier puts fixed bodies and idle
/// kinematic bodies to sleep too, which says nothing about simulation load.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PhysicsStats {
    /// Rigid bodies of every kind.
    pub bodies: usize,
    /// Dynamic bodies.
    pub dynamic: usize,
    /// Kinematic bodies (position- or velocity-based).
    pub kinematic: usize,
    /// Static (fixed) bodies.
    pub fixed: usize,
    /// Dynamic bodies that are simulated this step.
    pub awake_dynamic: usize,
    /// Dynamic bodies asleep (no solver cost until something wakes them).
    pub sleeping_dynamic: usize,
    /// Dynamic bodies with continuous collision detection enabled.
    pub ccd_bodies: usize,
    /// Colliders, sensors included.
    pub colliders: usize,
    /// Sensor colliders.
    pub sensors: usize,
    /// Impulse joints.
    pub joints: usize,
    /// Collider pairs with at least one active contact.
    pub contact_pairs: usize,
    /// Steps run since the world was created.
    pub steps: u64,
    /// Wall time of the last rapier step in milliseconds (0 before the first step).
    pub last_step_ms: f64,
    /// Non-finite body poses, body fields, velocities, forces and impulses dropped at the
    /// rapier seam since the world was created (ADR-1507). Zero in a healthy app; a growing
    /// count means a host script, a deserialized scene or a networked value is writing NaN
    /// or infinity into a `Transform`, `Velocity`, force or body field.
    pub non_finite_inputs: u64,
}

impl Default for PhysicsWorld {
    fn default() -> Self {
        Self {
            bodies: RigidBodySet::new(),
            colliders: ColliderSet::new(),
            impulse_joints: ImpulseJointSet::new(),
            multibody_joints: MultibodyJointSet::new(),
            soft_bodies: SoftBodySet::new(),
            islands: IslandManager::new(),
            broad_phase: BroadPhaseBvh::new(),
            narrow_phase: NarrowPhase::new(),
            ccd_solver: CCDSolver::new(),
            pipeline: PhysicsPipeline::new(),
            accumulator: 0.0,
            collider_entities: HashMap::new(),
            body_entities: HashMap::new(),
            body_handles: HashMap::new(),
            collider_handles: HashMap::new(),
            joint_handles: HashMap::new(),
            event_buffer: EventBuffer::default(),
            removed_awaiting_step: HashMap::new(),
            removed_stepped: HashMap::new(),
            broken_joints: HashSet::new(),
            step_count: 0,
            fixed_elapsed: std::time::Duration::ZERO,
            last_step_secs: 0.0,
            non_finite_inputs: 0,
            pending_removals: PendingRemovals::default(),
        }
    }
}

impl PhysicsWorld {
    /// The ECS entity that owns a collider, if any (specs/15 § 3 events/queries).
    pub fn entity_of_collider(&self, handle: ColliderHandle) -> Option<Entity> {
        self.collider_entities.get(&handle).copied()
    }

    /// The ECS entity that owns a rigid body, if any.
    pub fn entity_of_body(&self, handle: RigidBodyHandle) -> Option<Entity> {
        self.body_entities.get(&handle).copied()
    }

    /// The rapier body of an ECS entity, if any (removal bookkeeping).
    pub(crate) fn body_handle_of(&self, entity: Entity) -> Option<RigidBodyHandle> {
        self.body_handles.get(&entity).copied()
    }

    /// The rapier collider of an ECS entity, if any (removal bookkeeping).
    pub(crate) fn collider_handle_of(&self, entity: Entity) -> Option<ColliderHandle> {
        self.collider_handles.get(&entity).copied()
    }

    /// Records that `entity` owns `body` (called by the sync and the debris spawner).
    pub(crate) fn register_body(&mut self, handle: RigidBodyHandle, entity: Entity) {
        self.body_entities.insert(handle, entity);
        self.body_handles.insert(entity, handle);
    }

    /// Records that `entity` owns `collider` (called by the sync and the debris spawner).
    pub(crate) fn register_collider(&mut self, handle: ColliderHandle, entity: Entity) {
        self.collider_entities.insert(handle, entity);
        self.collider_handles.insert(entity, handle);
    }

    /// A query-pipeline view over this world with the given filter (specs/15 § 3).
    ///
    /// The view borrows the sets; build one per query (cheap — it is just references).
    pub fn query_pipeline<'a>(&'a self, filter: QueryFilter<'a>) -> QueryPipeline<'a> {
        self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            filter,
        )
    }

    /// A mutable query-pipeline view (character-controller pushes need it).
    pub fn query_pipeline_mut<'a>(&'a mut self, filter: QueryFilter<'a>) -> QueryPipelineMut<'a> {
        self.broad_phase.as_query_pipeline_mut(
            self.narrow_phase.query_dispatcher(),
            &mut self.bodies,
            &mut self.colliders,
            filter,
        )
    }

    /// Removes a body and its colliders from the rapier world, cleaning the entity maps.
    pub fn remove_body(&mut self, handle: RigidBodyHandle) {
        self.remove_body_listing_colliders(handle);
    }

    /// [`Self::remove_body`], returning the entities whose colliders went with the body (so
    /// the removal sync can strip their stale `ColliderHandle` components).
    pub(crate) fn remove_body_listing_colliders(&mut self, handle: RigidBodyHandle) -> Vec<Entity> {
        // The colliders attached to this body (removed with it), from the body's own list:
        // O(colliders on the body), so despawning thousands of bodies stays linear.
        let attached: Vec<ColliderHandle> = self
            .bodies
            .get(handle)
            .map(|rb| rb.colliders().to_vec())
            .unwrap_or_default();
        // Joints on this body go too — snapshot them first: rapier's removal already
        // drops the joints, so looking them up afterwards would come up empty. Most bodies
        // have none; skip the joint-map walk for them.
        let has_joints = self.impulse_joints.attached_joints(handle).next().is_some();
        let broken: Vec<(Entity, ImpulseJointHandle)> = if has_joints {
            self.joint_handles
                .iter()
                .filter(|(_, jh)| {
                    self.impulse_joints
                        .get(**jh)
                        .is_some_and(|j| j.body1() == handle || j.body2() == handle)
                })
                .map(|(e, jh)| (*e, *jh))
                .collect()
        } else {
            Vec::new()
        };
        let PhysicsWorld {
            bodies,
            islands,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            body_entities,
            body_handles,
            collider_entities,
            collider_handles,
            joint_handles,
            removed_awaiting_step,
            ..
        } = self;
        // Sensor flags must be read before the colliders go.
        let attached: Vec<(ColliderHandle, Option<bool>)> = attached
            .into_iter()
            .map(|ch| (ch, sensor_flag_before_removal(colliders.get(ch))))
            .collect();
        rapier3d::dynamics::RigidBodySet::remove(
            bodies,
            handle,
            islands,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            true,
        );
        if let Some(entity) = body_entities.remove(&handle) {
            body_handles.remove(&entity);
        }
        let mut collider_owners = Vec::with_capacity(attached.len());
        for (ch, sensor_flag) in attached {
            if let Some(entity) = collider_entities.remove(&ch) {
                collider_owners.push(entity);
                // Only drop the reverse entry if it still points at this collider (a rebuilt
                // collider may already own the entity's slot).
                if collider_handles.get(&entity) == Some(&ch) {
                    collider_handles.remove(&entity);
                }
                if let Some(sensor) = sensor_flag {
                    removed_awaiting_step.insert(ch, (entity, sensor));
                }
            }
        }
        // The joint entities keep their `Joint` component; `sync_joints` re-creates the
        // rapier joint once both bodies exist again.
        for (entity, jh) in broken {
            if impulse_joints.contains(jh) {
                impulse_joints.remove(jh, false);
            }
            joint_handles.remove(&entity);
        }
        collider_owners
    }

    /// Removes one collider from the rapier world and the entity map.
    pub fn remove_collider(&mut self, handle: ColliderHandle) {
        let PhysicsWorld {
            bodies,
            islands,
            colliders,
            soft_bodies,
            collider_entities,
            collider_handles,
            removed_awaiting_step,
            ..
        } = self;
        let sensor_flag = sensor_flag_before_removal(colliders.get(handle));
        if colliders.contains(handle) {
            colliders.remove(handle, islands, bodies, soft_bodies, false);
        }
        if let Some(entity) = collider_entities.remove(&handle) {
            if collider_handles.get(&entity) == Some(&handle) {
                collider_handles.remove(&entity);
            }
            // The next step reports this collider's `Stopped` pairs (rapier wakes the bodies
            // it touched); the flush maps them back to `entity` through this record.
            if let Some(sensor) = sensor_flag {
                removed_awaiting_step.insert(handle, (entity, sensor));
            }
        }
    }

    /// Removes a joint created for `entity` (joint component removed or entity despawned).
    pub(crate) fn remove_joint_of(&mut self, entity: Entity) {
        if let Some(handle) = self.joint_handles.remove(&entity)
            && self.impulse_joints.contains(handle)
        {
            self.impulse_joints.remove(handle, true);
        }
    }

    /// The entity and sensor flag of a collider named by a step event: live colliders
    /// first, then colliders removed before that step.
    pub(crate) fn event_participant(&self, handle: ColliderHandle) -> Option<(Entity, bool)> {
        if let Some(entity) = self.collider_entities.get(&handle) {
            let sensor = self.colliders.get(handle).is_some_and(|c| c.is_sensor());
            return Some((*entity, sensor));
        }
        self.removed_stepped.get(&handle).copied()
    }

    /// Simulated time in seconds: the sum of the fixed step lengths [`crate::step_physics`] has
    /// run. Debris lifetimes ([`crate::DebrisTag`]) are measured on this clock.
    #[must_use]
    pub fn fixed_elapsed_secs(&self) -> f64 {
        self.fixed_elapsed.as_secs_f64()
    }

    /// Counts of bodies, colliders, joints and contacts, plus the last step's cost.
    ///
    /// Walks every body, collider and contact pair: meant for diagnostics and the agent
    /// protocol, not for per-frame gameplay.
    pub fn stats(&self) -> PhysicsStats {
        let mut stats = PhysicsStats {
            bodies: self.bodies.len(),
            colliders: self.colliders.len(),
            joints: self.impulse_joints.len(),
            steps: self.step_count,
            last_step_ms: self.last_step_secs * 1000.0,
            non_finite_inputs: self.non_finite_inputs,
            ..PhysicsStats::default()
        };
        for (_, body) in self.bodies.iter() {
            if body.is_dynamic() {
                stats.dynamic += 1;
                if body.is_sleeping() {
                    stats.sleeping_dynamic += 1;
                } else {
                    stats.awake_dynamic += 1;
                }
                if body.is_ccd_enabled() {
                    stats.ccd_bodies += 1;
                }
            } else if body.is_kinematic() {
                stats.kinematic += 1;
            } else {
                stats.fixed += 1;
            }
        }
        stats.sensors = self.colliders.iter().filter(|(_, c)| c.is_sensor()).count();
        stats.contact_pairs = self
            .narrow_phase
            .contact_pairs()
            .filter(|pair| pair.has_any_active_contact())
            .count();
        stats
    }

    /// A 64-bit digest of the simulation state: every body's handle, pose, velocities and
    /// sleep flag, in rapier handle order, hashed bit-exactly (FNV-1a).
    ///
    /// Two runs with the same build, platform and inputs produce the same digest at the same
    /// step (specs/15 § 5); comparing digests is how the determinism tests and the agent's
    /// `physics.stats` detect divergence without shipping every pose. Equal digests do not
    /// prove equal states (it is a hash), but any bit of drift changes it with overwhelming
    /// probability.
    pub fn state_digest(&self) -> u64 {
        let mut hash = Fnv1a::default();
        for (handle, body) in self.bodies.iter() {
            let (index, generation) = handle.into_raw_parts();
            hash.write_u32(index);
            hash.write_u32(generation);
            let pose = body.position();
            let t = pose.translation;
            let r = pose.rotation;
            let lin = body.linvel();
            let ang = body.angvel();
            for value in [
                t.x, t.y, t.z, r.x, r.y, r.z, r.w, lin.x, lin.y, lin.z, ang.x, ang.y, ang.z,
            ] {
                hash.write_u32(value.to_bits());
            }
            hash.write_u32(u32::from(body.is_sleeping()));
        }
        hash.finish()
    }
}

/// `Some(is_sensor)` for an existing collider, read before it is removed: its `Stopped` pairs
/// (reported when either side of a pair has collision events) still need its entity.
fn sensor_flag_before_removal(collider: Option<&rapier3d::geometry::Collider>) -> Option<bool> {
    collider.map(|c| c.is_sensor())
}

/// FNV-1a, 64-bit (Fowler–Noll–Vo, public domain): a small, stable, platform-independent
/// hash for [`PhysicsWorld::state_digest`]. `std`'s `DefaultHasher` is not guaranteed stable
/// across Rust releases, and digests are compared across processes and builds.
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    fn write_u32(&mut self, value: u32) {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::prelude::Entity;

    /// Creates placeholder entities for budget testing.
    fn placeholder_entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("valid raw entity")
    }

    #[test]
    fn budget_enforces_cap() {
        let mut budget = BodyBudget::default();
        for i in 0u32..10 {
            budget.push(placeholder_entity(i));
        }
        assert_eq!(budget.len(), 10);
        let overflow = budget.enforce(5);
        assert_eq!(overflow.len(), 5);
        assert_eq!(budget.len(), 5);
        assert_eq!(overflow[0], placeholder_entity(0));
    }

    #[test]
    fn budget_remove_specific() {
        let mut budget = BodyBudget::default();
        let a = placeholder_entity(1);
        let b = placeholder_entity(2);
        let c = placeholder_entity(3);
        budget.push(a);
        budget.push(b);
        budget.push(c);
        budget.remove(b);
        assert_eq!(budget.len(), 2);
    }

    #[test]
    fn budget_empty() {
        let budget = BodyBudget::default();
        assert!(budget.is_empty());
        assert_eq!(budget.len(), 0);
    }

    #[test]
    fn physics_settings_defaults() {
        let s = PhysicsSettings::default();
        assert!((s.gravity - Vec3::new(0.0, -9.81, 0.0)).length() < 1e-6);
        assert_eq!(s.max_debris_bodies, 512);
        assert_eq!(s.fixed_hz, 60.0);
        // specs/15 § 5 (ADR-870): the solver defaults.
        assert_eq!(s.solver_iterations, 4);
        assert_eq!(s.ccd_substeps, 1);
    }

    #[test]
    fn remove_body_cleans_maps() {
        use rapier3d::dynamics::RigidBodyBuilder;
        let mut pw = PhysicsWorld::default();
        let bh = pw.bodies.insert(RigidBodyBuilder::dynamic().build());
        let ch = {
            let PhysicsWorld {
                ref mut bodies,
                ref mut colliders,
                ..
            } = pw;
            colliders.insert_with_parent(
                rapier3d::geometry::ColliderBuilder::cuboid(1.0, 1.0, 1.0).build(),
                bh,
                bodies,
            )
        };
        let e = placeholder_entity(9);
        pw.register_body(bh, e);
        pw.register_collider(ch, e);
        assert_eq!(pw.entity_of_body(bh), Some(e));
        assert_eq!(pw.entity_of_collider(ch), Some(e));

        pw.remove_body(bh);
        assert_eq!(pw.entity_of_body(bh), None);
        assert_eq!(pw.entity_of_collider(ch), None);
        assert_eq!(pw.bodies.len(), 0);
        assert_eq!(pw.colliders.len(), 0);
    }

    #[test]
    fn remove_body_also_removes_its_joints() {
        use rapier3d::dynamics::{RevoluteJoint, RigidBodyBuilder};
        let mut pw = PhysicsWorld::default();
        let b1 = pw.bodies.insert(RigidBodyBuilder::dynamic().build());
        let b2 = pw.bodies.insert(RigidBodyBuilder::dynamic().build());
        let jh = pw.impulse_joints.insert(
            b1,
            b2,
            RevoluteJoint::new(rapier3d::math::Vector::new(0.0, 1.0, 0.0)),
            true,
        );
        let joint_entity = placeholder_entity(42);
        pw.joint_handles.insert(joint_entity, jh);
        assert_eq!(pw.impulse_joints.len(), 1);

        pw.remove_body(b1);
        assert_eq!(pw.impulse_joints.len(), 0, "the joint went with the body");
        assert!(!pw.joint_handles.contains_key(&joint_entity));
    }
}
