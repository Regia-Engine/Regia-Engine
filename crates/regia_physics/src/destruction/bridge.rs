// SPDX-License-Identifier: MPL-2.0
//! The rapier side of destruction: the intact structure's colliders, the debris bodies of detached
//! chunks (pooled and capped), the sleep/lifetime/kill-height despawn rules, and the collider and
//! navmesh invalidation that follows a break.
//!
//! Each destructible's intact structure is one fixed body with one convex-hull collider per
//! attached chunk. When a chunk's [`SpawnRequest`] comes up (at most
//! [`DestructionBudget::max_spawns_per_tick`] per tick, oldest first), its collider leaves the
//! intact body, the region it covered is reported for navmesh rebuilds, and a dynamic body from the
//! pool takes its place with the decided velocities. Past [`DestructionBudget::max_active_chunks`]
//! the oldest debris goes back to the pool. Debris motion is rapier's: cosmetic, never fed back
//! into a break decision.
//!
//! With `FallingSettings` (ADR-2734) a falling section (chunks that left together, still bonded)
//! gets one dynamic body with one collider per chunk instead of a body each; rapier sums the chunks'
//! mass properties. Its bonds take damage from the section's own impacts (the change of momentum
//! between ticks that gravity does not explain) and, when one breaks, the section splits into its
//! bonded islands, each keeping the velocity of its centre of mass and the section's spin. Like all
//! debris motion this is cosmetic: every peer splits its own copy. On a server, an object with no
//! support chunks is a free dynamic body from its first tick; the sim follows its pose.

use std::collections::{BTreeMap, HashMap, VecDeque};

use rapier3d::dynamics::{RigidBodyBuilder, RigidBodyHandle, RigidBodyType};
use rapier3d::geometry::{ColliderBuilder, ColliderHandle};
use regia_core::math::{Quat, Vec3};

use super::sim::{
    ChunkState, DestructibleId, DestructibleInstance, DestructionBudget, DestructionError, DestructionWorld,
    SpawnRequest,
};
use crate::components::ColliderShape;
use crate::convert::{from_rapier_pose, to_rapier_vec3};
use crate::step::shape_to_shared_shape;
use crate::world::PhysicsWorld;

/// A world-space box whose navmesh tiles need rebuilding after chunks left the structure.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NavInvalidation {
    /// The instance.
    pub destructible: DestructibleId,
    /// Box minimum.
    pub min: [f32; 3],
    /// Box maximum.
    pub max: [f32; 3],
}

/// What one [`DestructionPhysics::sync`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BridgeReport {
    /// Chunks that got a debris body this tick.
    pub spawned: Vec<(DestructibleId, u32)>,
    /// Chunks whose debris body went back to the pool (or that never got one: a cap of 0).
    pub despawned: Vec<(DestructibleId, u32)>,
    /// Of `despawned`: recycled because the active cap was reached.
    pub evicted: u32,
    /// Of `despawned`: asleep too long, too old, or below the kill height.
    pub expired: u32,
    /// Intact-structure colliders removed this tick.
    pub colliders_removed: u32,
    /// Regions to rebuild, one merged box per instance that lost chunks, in id order.
    pub nav: Vec<NavInvalidation>,
    /// Falling sections that got their body this tick: `(instance, section)`.
    pub sections_spawned: Vec<(DestructibleId, u32)>,
    /// Sections that split this tick.
    pub splits: Vec<SectionSplit>,
}

/// A falling section that split into islands (ADR-2734).
#[derive(Clone, Debug, PartialEq)]
pub struct SectionSplit {
    /// The instance.
    pub destructible: DestructibleId,
    /// The section (its lowest chunk index).
    pub section: u32,
    /// Bonded islands it split into (single chunks become ordinary debris bodies).
    pub islands: u32,
    /// Linear momentum (kg m/s) of the section just before the split.
    pub momentum_before: [f32; 3],
    /// Summed linear momentum of the islands just after it.
    pub momentum_after: [f32; 3],
    /// Its chunks (each now in one of the islands' bodies, with a new collider).
    pub chunks: Vec<u32>,
}

/// One falling section's body (ADR-2734).
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSection {
    /// The instance.
    pub destructible: DestructibleId,
    /// The section (its lowest chunk index when it fell).
    pub section: u32,
    /// Its chunks, ascending.
    pub chunks: Vec<u32>,
    /// One entry per chunk, in `chunks` order: its collider, or `None` when its hull is one rapier
    /// cannot build (collinear or coincident points), like the intact structure's colliders.
    pub colliders: Vec<Option<ColliderHandle>>,
    /// The bonds still holding it together.
    pub bonds: Vec<u32>,
    /// Impact damage per bond, newton-seconds, in `bonds` order.
    pub bond_damage: Vec<f32>,
    /// Its rapier body.
    pub body: RigidBodyHandle,
    /// Seconds since it spawned.
    pub age_secs: f32,
    /// Seconds it has been asleep without waking.
    pub asleep_secs: f32,
    /// Linear velocity at the end of the last sync (impact detection).
    last_linvel: [f32; 3],
}

/// Running totals of the bridge.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BridgeStats {
    /// Debris bodies alive now.
    pub active: u32,
    /// Disabled bodies waiting in the pool.
    pub pool_free: u32,
    /// Debris bodies handed out since creation.
    pub spawned_total: u64,
    /// Of those, bodies taken from the pool instead of created.
    pub pool_reuses: u64,
    /// Debris bodies returned since creation.
    pub despawned_total: u64,
    /// Of those, recycled by the active cap.
    pub evicted_total: u64,
    /// Intact colliders removed since creation.
    pub colliders_removed_total: u64,
    /// Detached chunks waiting for a body (from the last sync).
    pub pending_spawns: u32,
    /// Microseconds the last sync took (informational; never used in a decision).
    pub last_sync_us: u64,
    /// Falling-section bodies alive now (they count against `max_active_chunks`).
    pub sections: u32,
    /// Section splits since creation.
    pub splits_total: u64,
    /// Instances simulated as free bodies now.
    pub free_bodies: u32,
}

/// One live debris body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveChunk {
    /// The instance.
    pub destructible: DestructibleId,
    /// Chunk index.
    pub chunk: u32,
    /// Its rapier body.
    pub body: RigidBodyHandle,
    /// Its rapier collider.
    pub collider: ColliderHandle,
    /// Seconds since it spawned.
    pub age_secs: f32,
    /// Seconds it has been asleep without waking.
    pub asleep_secs: f32,
    /// The chunk's own lifetime (`DestructibleChunk::lifetime_secs`), overriding the budget's.
    pub lifetime_secs: Option<f32>,
}

#[derive(Clone, Debug)]
struct IntactBody {
    body: RigidBodyHandle,
    colliders: Vec<Option<ColliderHandle>>,
    /// A free dynamic body (no support chunks, `FallingSettings::free_body`, on a server).
    free: bool,
}

/// The rapier side of a [`DestructionWorld`].
#[derive(Clone, Debug, Default)]
pub struct DestructionPhysics {
    intact: BTreeMap<DestructibleId, IntactBody>,
    active: VecDeque<ActiveChunk>,
    sections: VecDeque<ActiveSection>,
    pool: Vec<RigidBodyHandle>,
    collider_chunks: HashMap<ColliderHandle, (DestructibleId, u32)>,
    /// Asset-space centroids of section chunks (their pose inside the section body).
    chunk_centroids: HashMap<(DestructibleId, u32), [f32; 3]>,
    /// Gravity for the section impact test (`None`: Earth's, along -Y).
    gravity: Option<[f32; 3]>,
    stats: BridgeStats,
}

/// What a section body is built from.
struct SectionSpec {
    destructible: DestructibleId,
    section: u32,
    chunks: Vec<u32>,
    bonds: Vec<u32>,
    translation: [f32; 3],
    rotation: [f32; 4],
    linvel: [f32; 3],
    angvel: [f32; 3],
}

impl DestructionPhysics {
    /// Builds the intact structure of instance `id`: one fixed body at its pose and one hull
    /// collider per attached chunk. Calling it again for the same id does nothing.
    ///
    /// # Errors
    /// [`DestructionError::UnknownId`] when the world has no such instance.
    pub fn attach(
        &mut self,
        physics: &mut PhysicsWorld,
        world: &DestructionWorld,
        id: DestructibleId,
    ) -> Result<(), DestructionError> {
        if self.intact.contains_key(&id) {
            return Ok(());
        }
        let inst = world.instance(id).ok_or(DestructionError::UnknownId(id.0))?;
        let pose = inst.pose();
        // A free object is a dynamic body on the server; a replica follows the pose its game
        // replicates, as for any moving structure.
        let free = world.authority() == super::sim::Authority::Server
            && inst.asset().falling.as_ref().is_some_and(|f| f.free_body)
            && !inst.asset().chunks.iter().any(|c| c.support);
        let kind = if free {
            RigidBodyType::Dynamic
        } else {
            RigidBodyType::Fixed
        };
        // The pose, not translation + rotation(axis-angle): the axis-angle round trip goes
        // through platform libm (`atan2`, then rapier's `sin`/`cos`) and would store different
        // rotation bits per platform, while `state_digest` hashes this body like every other
        // (spec 15 § 5, cross-platform lockstep).
        let body = RigidBodyBuilder::new(kind)
            .pose(crate::convert::to_rapier_pose(
                Vec3::from_array(pose.translation),
                Quat::from_array(pose.rotation),
            ))
            .can_sleep(true)
            .build();
        let body = physics.bodies.insert(body);
        let mut colliders = Vec::with_capacity(inst.asset().chunks.len());
        for (c, chunk) in inst.asset().chunks.iter().enumerate() {
            if inst.chunk_states()[c] != ChunkState::Attached {
                colliders.push(None);
                continue;
            }
            let handle = insert_hull(
                physics,
                body,
                chunk.collider_shape(),
                Some(chunk.centroid),
                Some(chunk.mass),
            );
            if let Some(h) = handle {
                self.collider_chunks.insert(h, (id, c as u32));
            }
            colliders.push(handle);
        }
        self.intact.insert(
            id,
            IntactBody {
                body,
                colliders,
                free,
            },
        );
        self.refresh_counts();
        Ok(())
    }

    /// Removes instance `id` from rapier: its intact body and every debris body it has (debris
    /// bodies go back to the pool). Returns the chunks whose debris went.
    pub fn detach(&mut self, physics: &mut PhysicsWorld, id: DestructibleId) -> Vec<(DestructibleId, u32)> {
        if let Some(intact) = self.intact.remove(&id) {
            for h in intact.colliders.iter().flatten() {
                self.collider_chunks.remove(h);
            }
            physics.remove_body(intact.body);
        }
        let mut gone = Vec::new();
        let mut keep = VecDeque::with_capacity(self.active.len());
        while let Some(a) = self.active.pop_front() {
            if a.destructible == id {
                self.release(physics, a);
                gone.push((a.destructible, a.chunk));
            } else {
                keep.push_back(a);
            }
        }
        self.active = keep;
        let mut keep = VecDeque::with_capacity(self.sections.len());
        while let Some(sec) = self.sections.pop_front() {
            if sec.destructible == id {
                gone.extend(sec.chunks.iter().map(|c| (id, *c)));
                self.release_section(physics, sec);
            } else {
                keep.push_back(sec);
            }
        }
        self.sections = keep;
        self.refresh_counts();
        gone
    }

    /// One fixed tick: expire debris (sleep, lifetime, kill height), then give detached chunks
    /// their bodies (at most `max_spawns_per_tick`, recycling the oldest past `max_active_chunks`)
    /// and remove their intact colliders.
    #[tracing::instrument(skip_all, target = "regia_physics", name = "destruction_physics_sync")]
    pub fn sync(
        &mut self,
        physics: &mut PhysicsWorld,
        world: &mut DestructionWorld,
        dt: f32,
    ) -> BridgeReport {
        let started = std::time::Instant::now();
        let budget = world.budget().clone();
        let mut report = BridgeReport::default();
        self.expire(physics, &budget, dt, &mut report);

        self.follow_free_bodies(physics, world);
        self.impact_sections(physics, world, &budget, dt, &mut report);
        let groups = world.take_spawn_groups(budget.max_spawns_per_tick as usize);
        let mut regions: BTreeMap<DestructibleId, ([f32; 3], [f32; 3])> = BTreeMap::new();
        let mut spawns = Vec::new();
        for group in groups {
            if group.len() < 2 {
                spawns.extend(group);
                continue;
            }
            let Some(inst) = world.instance(group[0].destructible) else {
                continue;
            };
            for request in &group {
                self.remove_intact_collider(physics, request, &mut report);
                let (lo, hi) = world_bounds(inst, request.chunk as usize);
                merge_region(&mut regions, request.destructible, lo, hi);
            }
            if budget.max_active_chunks == 0 {
                report
                    .despawned
                    .extend(group.iter().map(|r| (r.destructible, r.chunk)));
                continue;
            }
            self.make_room(physics, &budget, &mut report);
            if let Some(section) = self.spawn_section(physics, inst, &group) {
                report
                    .sections_spawned
                    .push((section.destructible, section.section));
                report
                    .spawned
                    .extend(section.chunks.iter().map(|c| (section.destructible, *c)));
                self.sections.push_back(section);
            }
        }
        for request in spawns {
            let Some(inst) = world.instance(request.destructible) else {
                continue;
            };
            let c = request.chunk as usize;
            self.remove_intact_collider(physics, &request, &mut report);
            let (lo, hi) = world_bounds(inst, c);
            merge_region(&mut regions, request.destructible, lo, hi);
            // A cap of 0, or a small chunk: no body, the effect hook is all there is.
            let small = inst.asset().chunks.get(c).is_some_and(|ch| ch.small);
            if budget.max_active_chunks == 0 || small {
                report.despawned.push((request.destructible, request.chunk));
                continue;
            }
            self.make_room(physics, &budget, &mut report);
            if let Some(active) = self.spawn(physics, inst, &request, true) {
                report.spawned.push((request.destructible, request.chunk));
                self.active.push_back(active);
            }
        }
        // The pool never holds more than the cap allows.
        while self.pool.len() + self.bodies_alive() > budget.max_active_chunks as usize {
            let Some(body) = self.pool.pop() else {
                break;
            };
            physics.remove_body(body);
        }
        report.nav = regions
            .into_iter()
            .map(|(destructible, (min, max))| NavInvalidation {
                destructible,
                min,
                max,
            })
            .collect();
        self.stats.spawned_total += report.spawned.len() as u64;
        self.stats.despawned_total += report.despawned.len() as u64;
        self.stats.evicted_total += u64::from(report.evicted);
        self.stats.colliders_removed_total += u64::from(report.colliders_removed);
        self.stats.pending_spawns = world.pending_spawns() as u32;
        self.refresh_counts();
        self.stats.last_sync_us = started.elapsed().as_micros() as u64;
        report
    }

    /// Running totals.
    #[must_use]
    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }

    /// Live debris bodies, oldest first.
    pub fn active(&self) -> impl Iterator<Item = &ActiveChunk> {
        self.active.iter()
    }

    /// The instance and chunk a collider belongs to (intact or debris): map a game raycast hit to
    /// a [`super::DamageKind::Impulse`] target.
    #[must_use]
    pub fn chunk_of_collider(&self, handle: ColliderHandle) -> Option<(DestructibleId, u32)> {
        self.collider_chunks.get(&handle).copied()
    }

    /// Moves the intact structure of `id` to the world's current pose of the instance
    /// ([`DestructionWorld::set_pose`]): the body turns kinematic on its first move and reaches
    /// the pose at the next step, carrying whatever rests on it.
    ///
    /// # Errors
    /// [`DestructionError::UnknownId`] when the world or the bridge has no such instance.
    pub fn follow_pose(
        &mut self,
        physics: &mut PhysicsWorld,
        world: &DestructionWorld,
        id: DestructibleId,
    ) -> Result<(), DestructionError> {
        let pose = world
            .instance(id)
            .ok_or(DestructionError::UnknownId(id.0))?
            .pose();
        let body = self.intact_body(id).ok_or(DestructionError::UnknownId(id.0))?;
        // A free body moves itself; the world follows it, not the other way round.
        if self.intact.get(&id).is_some_and(|i| i.free) {
            return Ok(());
        }
        if let Some(rb) = physics.bodies.get_mut(body) {
            if rb.is_fixed() {
                rb.set_body_type(RigidBodyType::KinematicPositionBased, true);
            }
            rb.set_next_kinematic_position(crate::convert::to_rapier_pose(
                Vec3::from_array(pose.translation),
                Quat::from_array(pose.rotation),
            ));
        }
        Ok(())
    }

    /// The intact structure's rapier body of an instance.
    #[must_use]
    pub fn intact_body(&self, id: DestructibleId) -> Option<RigidBodyHandle> {
        self.intact.get(&id).map(|i| i.body)
    }

    /// The intact collider of a chunk (`None` once it detached).
    #[must_use]
    pub fn intact_collider(&self, id: DestructibleId, chunk: u32) -> Option<ColliderHandle> {
        self.intact
            .get(&id)
            .and_then(|i| i.colliders.get(chunk as usize).copied().flatten())
    }

    /// World pose of a chunk's debris body, if it has one.
    #[must_use]
    pub fn debris_pose(
        &self,
        physics: &PhysicsWorld,
        id: DestructibleId,
        chunk: u32,
    ) -> Option<(Vec3, Quat)> {
        if let Some(a) = self
            .active
            .iter()
            .find(|a| a.destructible == id && a.chunk == chunk)
        {
            let rb = physics.bodies.get(a.body)?;
            return Some(from_rapier_pose(rb.position()));
        }
        // A chunk of a falling section: the section body's frame is the asset's.
        let sec = self
            .sections
            .iter()
            .find(|s| s.destructible == id && s.chunks.contains(&chunk))?;
        let rb = physics.bodies.get(sec.body)?;
        let (t, q) = from_rapier_pose(rb.position());
        let centroid = self.chunk_centroids.get(&(id, chunk)).copied()?;
        Some((t + q * Vec3::from_array(centroid), q))
    }

    /// The debris collider of a chunk (its own body's, or its section's); `None` while it has none.
    #[must_use]
    pub fn debris_collider(&self, id: DestructibleId, chunk: u32) -> Option<ColliderHandle> {
        if let Some(a) = self
            .active
            .iter()
            .find(|a| a.destructible == id && a.chunk == chunk)
        {
            return Some(a.collider);
        }
        self.sections
            .iter()
            .filter(|s| s.destructible == id)
            .find_map(|s| {
                let i = s.chunks.iter().position(|c| *c == chunk)?;
                s.colliders.get(i).copied().flatten()
            })
    }

    /// World poses of the free-body instances' structures: `(instance, translation, rotation)`.
    pub fn free_body_poses<'a>(
        &'a self,
        physics: &'a PhysicsWorld,
    ) -> impl Iterator<Item = (DestructibleId, Vec3, Quat)> + 'a {
        self.intact.iter().filter(|(_, i)| i.free).filter_map(|(id, i)| {
            let (t, q) = from_rapier_pose(physics.bodies.get(i.body)?.position());
            Some((*id, t, q))
        })
    }

    /// Live falling-section bodies, oldest first (ADR-2734).
    pub fn sections(&self) -> impl Iterator<Item = &ActiveSection> {
        self.sections.iter()
    }

    /// Whether instance `id`'s intact structure is a free dynamic body (ADR-2734).
    #[must_use]
    pub fn is_free_body(&self, id: DestructibleId) -> bool {
        self.intact.get(&id).is_some_and(|i| i.free)
    }

    /// Sets the gravity the section impact test subtracts (m/s^2; default `[0, -9.81, 0]`). The
    /// plugin copies the physics gravity here each tick.
    pub fn set_gravity(&mut self, gravity: [f32; 3]) {
        self.gravity = Some(gravity);
    }

    fn bodies_alive(&self) -> usize {
        self.active.len() + self.sections.len()
    }

    /// Returns the oldest debris to the pool until one more body fits under the cap: single chunks
    /// first, then the oldest section.
    fn make_room(
        &mut self,
        physics: &mut PhysicsWorld,
        budget: &DestructionBudget,
        report: &mut BridgeReport,
    ) {
        while self.bodies_alive() >= budget.max_active_chunks as usize {
            if let Some(old) = self.active.pop_front() {
                self.release(physics, old);
                report.despawned.push((old.destructible, old.chunk));
                report.evicted += 1;
            } else if let Some(sec) = self.sections.pop_front() {
                report
                    .despawned
                    .extend(sec.chunks.iter().map(|c| (sec.destructible, *c)));
                report.evicted += 1;
                self.release_section(physics, sec);
            } else {
                break;
            }
        }
    }

    fn remove_intact_collider(
        &mut self,
        physics: &mut PhysicsWorld,
        request: &SpawnRequest,
        report: &mut BridgeReport,
    ) {
        if let Some(intact) = self.intact.get_mut(&request.destructible)
            && let Some(slot) = intact.colliders.get_mut(request.chunk as usize)
            && let Some(handle) = slot.take()
        {
            self.collider_chunks.remove(&handle);
            physics.remove_collider(handle);
            report.colliders_removed += 1;
        }
    }

    /// A body from the pool, or a new dynamic one, at `pose` with these velocities.
    fn take_body(
        &mut self,
        physics: &mut PhysicsWorld,
        translation: [f32; 3],
        rotation: [f32; 4],
        linvel: rapier3d::math::Vector,
        angvel: [f32; 3],
    ) -> RigidBodyHandle {
        let t = to_rapier_vec3(Vec3::from_array(translation));
        match self.pool.pop() {
            Some(handle) if physics.bodies.contains(handle) => {
                if let Some(rb) = physics.bodies.get_mut(handle) {
                    rb.set_enabled(true);
                    rb.set_translation(t, false);
                    rb.set_rotation(
                        crate::convert::to_rapier_rotation(Quat::from_array(rotation)),
                        false,
                    );
                    rb.set_linvel(linvel, false);
                    rb.set_angvel(to_rapier_vec3(Vec3::from_array(angvel)), false);
                    rb.reset_forces(false);
                    rb.reset_torques(false);
                    rb.wake_up(true);
                }
                self.stats.pool_reuses += 1;
                handle
            }
            _ => physics.bodies.insert(
                RigidBodyBuilder::new(RigidBodyType::Dynamic)
                    .pose(crate::convert::to_rapier_pose(
                        Vec3::from_array(translation),
                        Quat::from_array(rotation),
                    ))
                    .linvel(linvel)
                    .angvel(to_rapier_vec3(Vec3::from_array(angvel)))
                    .can_sleep(true)
                    .build(),
            ),
        }
    }

    /// One body for a falling section: the instance's frame, a collider per chunk at its centroid
    /// with its mass (and given inertia), the chunks' momentum-weighted velocity.
    fn spawn_section(
        &mut self,
        physics: &mut PhysicsWorld,
        inst: &DestructibleInstance,
        group: &[SpawnRequest],
    ) -> Option<ActiveSection> {
        let first = group.first()?;
        let pose = inst.pose();
        let mut mass = 0.0f32;
        let mut momentum = [0.0f32; 3];
        let mut spin = [0.0f32; 3];
        for r in group {
            let m = inst.asset().chunks.get(r.chunk as usize)?.mass;
            mass += m;
            for ((p, s), (v, w)) in momentum
                .iter_mut()
                .zip(&mut spin)
                .zip(r.linvel.iter().zip(&r.angvel))
            {
                *p += m * v;
                *s += m * w;
            }
        }
        let linvel = [momentum[0] / mass, momentum[1] / mass, momentum[2] / mass];
        let angvel = [spin[0] / mass, spin[1] / mass, spin[2] / mass];
        let chunks: Vec<u32> = group.iter().map(|r| r.chunk).collect();
        let bonds = inst
            .section_bonds(first.section)
            .map(<[u32]>::to_vec)
            .unwrap_or_default();
        self.build_section(
            physics,
            inst,
            SectionSpec {
                destructible: first.destructible,
                section: first.section,
                chunks,
                bonds,
                translation: pose.translation,
                rotation: pose.rotation,
                linvel,
                angvel,
            },
            None,
        )
    }

    /// A section body. `world_linvel` is the centre-of-mass velocity of a piece split off a
    /// section; `None` uses the spec's velocity plus a moving structure's velocity.
    fn build_section(
        &mut self,
        physics: &mut PhysicsWorld,
        inst: &DestructibleInstance,
        spec: SectionSpec,
        world_linvel: Option<rapier3d::math::Vector>,
    ) -> Option<ActiveSection> {
        let linvel = world_linvel.unwrap_or_else(|| {
            // A moving structure's sections keep the structure's velocity at their origin.
            let carried = self
                .intact
                .get(&spec.destructible)
                .and_then(|i| physics.bodies.get(i.body))
                .map_or(rapier3d::math::Vector::ZERO, |rb| {
                    rb.velocity_at_point(to_rapier_vec3(Vec3::from_array(spec.translation)))
                });
            to_rapier_vec3(Vec3::from_array(spec.linvel)) + carried
        });
        let body = self.take_body(physics, spec.translation, spec.rotation, linvel, spec.angvel);
        let mut colliders = Vec::with_capacity(spec.chunks.len());
        for &c in &spec.chunks {
            let chunk = inst.asset().chunks.get(c as usize)?;
            // A hull rapier cannot build keeps its slot, so `colliders` stays aligned with
            // `chunks` (corr-physics audit: a skipped slot shifted every later chunk's collider
            // and indexed past the end for the last one).
            let Some(shared) = shape_to_shared_shape(&chunk.collider_shape()) else {
                colliders.push(None);
                continue;
            };
            let mut builder =
                ColliderBuilder::new(shared).translation(to_rapier_vec3(Vec3::from_array(chunk.centroid)));
            builder = match chunk.inertia {
                Some(i) => builder.mass_properties(rapier3d::dynamics::MassProperties::new(
                    rapier3d::math::Vector::ZERO,
                    chunk.mass,
                    to_rapier_vec3(Vec3::from_array(i)),
                )),
                None => builder.mass(chunk.mass),
            };
            let PhysicsWorld {
                bodies,
                colliders: set,
                ..
            } = physics;
            let handle = set.insert_with_parent(builder.build(), body, bodies);
            self.collider_chunks.insert(handle, (spec.destructible, c));
            self.chunk_centroids
                .insert((spec.destructible, c), chunk.centroid);
            colliders.push(Some(handle));
        }
        let PhysicsWorld {
            bodies,
            colliders: set,
            ..
        } = physics;
        if let Some(rb) = bodies.get_mut(body) {
            rb.set_additional_mass_properties(rapier3d::dynamics::MassProperties::default(), false);
            rb.recompute_mass_properties_from_colliders(set);
            // The velocity was set before the colliders moved the centre of mass.
            rb.set_linvel(linvel, true);
        }
        Some(ActiveSection {
            destructible: spec.destructible,
            section: spec.section,
            chunks: spec.chunks,
            colliders,
            bond_damage: vec![0.0; spec.bonds.len()],
            bonds: spec.bonds,
            body,
            age_secs: 0.0,
            asleep_secs: 0.0,
            last_linvel: [linvel.x, linvel.y, linvel.z],
        })
    }

    fn release_section(&mut self, physics: &mut PhysicsWorld, sec: ActiveSection) {
        for (h, c) in sec.colliders.iter().zip(&sec.chunks) {
            if let Some(h) = h {
                self.collider_chunks.remove(h);
                physics.remove_collider(*h);
            }
            self.chunk_centroids.remove(&(sec.destructible, *c));
        }
        self.disable(physics, sec.body);
    }

    /// A free body's world: the sim follows the body's pose (server only; hits use it).
    fn follow_free_bodies(&mut self, physics: &PhysicsWorld, world: &mut DestructionWorld) {
        for (id, intact) in &self.intact {
            if !intact.free {
                continue;
            }
            let Some(rb) = physics.bodies.get(intact.body) else {
                continue;
            };
            let (t, q) = from_rapier_pose(rb.position());
            let pose = super::sim::DestructiblePose {
                translation: t.to_array(),
                rotation: q.to_array(),
            };
            if world.instance(*id).is_some_and(|i| i.pose() != pose) {
                // The pose comes from rapier, so it is finite.
                let _ = world.set_pose(*id, pose);
            }
        }
    }

    /// Impact damage on section bonds, and splits (cosmetic, per peer).
    fn impact_sections(
        &mut self,
        physics: &mut PhysicsWorld,
        world: &DestructionWorld,
        budget: &DestructionBudget,
        dt: f32,
        report: &mut BridgeReport,
    ) {
        let gravity = self.gravity.unwrap_or([0.0, -9.81, 0.0]);
        let mut index = 0;
        while index < self.sections.len() {
            let sec = &mut self.sections[index];
            let Some(inst) = world.instance(sec.destructible) else {
                index += 1;
                continue;
            };
            let Some(rb) = physics.bodies.get(sec.body) else {
                index += 1;
                continue;
            };
            let v = rb.linvel();
            let now = [v.x, v.y, v.z];
            let mass = rb.mass();
            let raw: f32 = (0..3)
                .map(|k| {
                    let d = now[k] - sec.last_linvel[k] - gravity[k] * dt;
                    d * d
                })
                .sum::<f32>()
                .sqrt();
            // A section at rest is held up by `-g dt` every step; that support is not an impact
            // (corr-physics audit: it added the section's weight times dt to its bonds each tick,
            // so a landed section split by itself). Same expression as `raw` at rest, so a
            // resting section's jolt is exactly 0.
            let support: f32 = (0..3)
                .map(|k| {
                    let d = gravity[k] * dt;
                    d * d
                })
                .sum::<f32>()
                .sqrt();
            let jolt = (raw - support).max(0.0);
            sec.last_linvel = now;
            let material = &inst.asset().material;
            let impulse = mass * jolt * material.damage_scale;
            let mut broke = false;
            if impulse > 0.0 && impulse >= material.min_impulse && !sec.bonds.is_empty() {
                let share = impulse / sec.bonds.len() as f32;
                for (d, b) in sec.bond_damage.iter_mut().zip(&sec.bonds) {
                    *d += share;
                    broke |= *d >= inst.asset().bonds[*b as usize].break_impulse;
                }
            }
            if !broke {
                index += 1;
                continue;
            }
            let Some(sec) = self.sections.remove(index) else {
                break;
            };
            self.split_section(physics, inst, sec, budget, report);
        }
    }

    fn split_section(
        &mut self,
        physics: &mut PhysicsWorld,
        inst: &DestructibleInstance,
        sec: ActiveSection,
        budget: &DestructionBudget,
        report: &mut BridgeReport,
    ) {
        let asset = inst.asset();
        let Some(rb) = physics.bodies.get(sec.body) else {
            return;
        };
        let (t, q) = from_rapier_pose(rb.position());
        let v = rb.linvel();
        let w = rb.angvel();
        let com = rb.center_of_mass();
        let before = rb.mass() * v;
        let keep: Vec<u32> = sec
            .bonds
            .iter()
            .zip(&sec.bond_damage)
            .filter(|(b, d)| **d < asset.bonds[**b as usize].break_impulse)
            .map(|(b, _)| *b)
            .collect();
        // Islands over the bonds that held, in chunk order.
        let mut island = vec![u32::MAX; sec.chunks.len()];
        let mut count = 0u32;
        for start in 0..sec.chunks.len() {
            if island[start] != u32::MAX {
                continue;
            }
            island[start] = count;
            let mut stack = vec![start];
            while let Some(i) = stack.pop() {
                for b in &keep {
                    let spec = &asset.bonds[*b as usize];
                    let other = if spec.a == sec.chunks[i] {
                        spec.b
                    } else if spec.b == sec.chunks[i] {
                        spec.a
                    } else {
                        continue;
                    };
                    if let Some(j) = sec.chunks.iter().position(|c| *c == other)
                        && island[j] == u32::MAX
                    {
                        island[j] = count;
                        stack.push(j);
                    }
                }
            }
            count += 1;
        }
        let (destructible, section) = (sec.destructible, sec.section);
        let chunks = sec.chunks.clone();
        self.release_section(physics, sec);
        let all = chunks.clone();
        let mut after = rapier3d::math::Vector::ZERO;
        for label in 0..count {
            let members: Vec<u32> = all
                .iter()
                .zip(&island)
                .filter(|(_, l)| **l == label)
                .map(|(c, _)| *c)
                .collect();
            // The island's centre of mass, and the section's velocity there.
            let mut m = 0.0f32;
            let mut sum = Vec3::ZERO;
            for c in &members {
                let chunk = &asset.chunks[*c as usize];
                m += chunk.mass;
                sum += chunk.mass * (t + q * Vec3::from_array(chunk.centroid));
            }
            let centre = sum / m;
            let at = to_rapier_vec3(centre);
            let lin = v + w.cross(at - com);
            let lin3 = [lin.x, lin.y, lin.z];
            let ang = [w.x, w.y, w.z];
            self.make_room(physics, budget, report);
            if let [c] = members.as_slice() {
                let request = SpawnRequest {
                    destructible,
                    chunk: *c,
                    position: (t + q * Vec3::from_array(asset.chunks[*c as usize].centroid)).to_array(),
                    rotation: q.to_array(),
                    linvel: lin3,
                    angvel: ang,
                    tick: 0,
                    section: *c,
                };
                if let Some(active) = self.spawn(physics, inst, &request, false) {
                    if let Some(rb) = physics.bodies.get(active.body) {
                        after += rb.mass() * rb.linvel();
                    }
                    self.active.push_back(active);
                }
                continue;
            }
            let bonds = keep
                .iter()
                .copied()
                .filter(|b| {
                    let spec = &asset.bonds[*b as usize];
                    members.contains(&spec.a) && members.contains(&spec.b)
                })
                .collect();
            let spec = SectionSpec {
                destructible,
                section: members[0],
                chunks: members,
                bonds,
                translation: t.to_array(),
                rotation: q.to_array(),
                linvel: [0.0; 3],
                angvel: ang,
            };
            if let Some(mut piece) = self.build_section(physics, inst, spec, Some(lin)) {
                if let Some(rb) = physics.bodies.get(piece.body) {
                    after += rb.mass() * rb.linvel();
                    piece.last_linvel = lin3;
                }
                self.sections.push_back(piece);
            }
        }
        self.stats.splits_total += 1;
        report.splits.push(SectionSplit {
            destructible,
            section,
            islands: count,
            chunks,
            momentum_before: [before.x, before.y, before.z],
            momentum_after: [after.x, after.y, after.z],
        });
    }

    fn expire(
        &mut self,
        physics: &mut PhysicsWorld,
        budget: &DestructionBudget,
        dt: f32,
        report: &mut BridgeReport,
    ) {
        // Rotate in place. `pop_front` + `push_back` reuses the deque's allocation when
        // its capacity already holds every live chunk. Order of survivors stays oldest-first.
        let active_n = self.active.len();
        for _ in 0..active_n {
            let Some(mut a) = self.active.pop_front() else {
                break;
            };
            let (sleeping, y) = physics
                .bodies
                .get(a.body)
                .map_or((true, f32::NEG_INFINITY), |rb| {
                    (rb.is_sleeping(), rb.translation().y)
                });
            a.age_secs += dt;
            a.asleep_secs = if sleeping { a.asleep_secs + dt } else { 0.0 };
            let lifetime = a.lifetime_secs.unwrap_or(budget.max_lifetime_secs);
            let expired = (budget.sleep_despawn_secs > 0.0 && a.asleep_secs >= budget.sleep_despawn_secs)
                || (lifetime > 0.0 && a.age_secs >= lifetime)
                || y < budget.kill_y;
            if expired {
                self.release(physics, a);
                report.despawned.push((a.destructible, a.chunk));
                report.expired += 1;
            } else {
                self.active.push_back(a);
            }
        }
        let section_n = self.sections.len();
        for _ in 0..section_n {
            let Some(mut sec) = self.sections.pop_front() else {
                break;
            };
            let (sleeping, y) = physics
                .bodies
                .get(sec.body)
                .map_or((true, f32::NEG_INFINITY), |rb| {
                    (rb.is_sleeping(), rb.center_of_mass().y)
                });
            sec.age_secs += dt;
            sec.asleep_secs = if sleeping { sec.asleep_secs + dt } else { 0.0 };
            let lifetime = budget.max_lifetime_secs;
            let expired = (budget.sleep_despawn_secs > 0.0 && sec.asleep_secs >= budget.sleep_despawn_secs)
                || (lifetime > 0.0 && sec.age_secs >= lifetime)
                || y < budget.kill_y;
            if expired {
                report
                    .despawned
                    .extend(sec.chunks.iter().map(|c| (sec.destructible, *c)));
                report.expired += 1;
                self.release_section(physics, sec);
            } else {
                self.sections.push_back(sec);
            }
        }
    }

    /// A debris body for `request`: from the pool when one is free.
    fn spawn(
        &mut self,
        physics: &mut PhysicsWorld,
        inst: &DestructibleInstance,
        request: &SpawnRequest,
        carry: bool,
    ) -> Option<ActiveChunk> {
        let c = request.chunk as usize;
        let chunk = inst.asset().chunks.get(c)?;
        let mass = chunk.mass;
        // A moving structure's debris keeps the structure's velocity at its point (cosmetic: the
        // decided velocity in the event is relative to the structure).
        let carried = self
            .intact
            .get(&request.destructible)
            .filter(|_| carry)
            .and_then(|i| physics.bodies.get(i.body))
            .map_or(rapier3d::math::Vector::ZERO, |rb| {
                rb.velocity_at_point(to_rapier_vec3(Vec3::from_array(request.position)))
            });
        let linvel = to_rapier_vec3(Vec3::from_array(request.linvel)) + carried;
        // The pose, not the axis-angle round trip: the round trip uses platform libm and the
        // pooled branch above already bit-copies, so a fresh and a pooled body for the same
        // chunk would otherwise store different rotation bits (spec 15 § 5).
        let translation = to_rapier_vec3(Vec3::from_array(request.position));
        let body = match self.pool.pop() {
            Some(handle) if physics.bodies.contains(handle) => {
                if let Some(rb) = physics.bodies.get_mut(handle) {
                    rb.set_enabled(true);
                    rb.set_translation(translation, false);
                    rb.set_rotation(
                        crate::convert::to_rapier_rotation(Quat::from_array(request.rotation)),
                        false,
                    );
                    rb.set_linvel(linvel, false);
                    rb.set_angvel(to_rapier_vec3(Vec3::from_array(request.angvel)), false);
                    rb.reset_forces(false);
                    rb.reset_torques(false);
                    rb.wake_up(true);
                }
                self.stats.pool_reuses += 1;
                handle
            }
            _ => physics.bodies.insert(
                RigidBodyBuilder::new(RigidBodyType::Dynamic)
                    .pose(crate::convert::to_rapier_pose(
                        Vec3::from_array(request.position),
                        Quat::from_array(request.rotation),
                    ))
                    .linvel(linvel)
                    .angvel(to_rapier_vec3(Vec3::from_array(request.angvel)))
                    .can_sleep(true)
                    .build(),
            ),
        };
        // Given inertia: a massless collider and the chunk's own mass properties on the body.
        // Otherwise the collider carries the mass and rapier derives the inertia from the hulls.
        // Either way the body's extra properties are reset (a pooled body may carry old ones).
        let (collider_mass, extra) = match chunk.inertia {
            Some(i) => (
                None,
                rapier3d::dynamics::MassProperties::new(
                    rapier3d::math::Vector::ZERO,
                    mass,
                    to_rapier_vec3(Vec3::from_array(i)),
                ),
            ),
            None => (Some(mass), rapier3d::dynamics::MassProperties::default()),
        };
        let Some(collider) = insert_hull(physics, body, chunk.collider_shape(), None, collider_mass) else {
            self.disable(physics, body);
            return None;
        };
        let PhysicsWorld {
            bodies, colliders, ..
        } = physics;
        if let Some(rb) = bodies.get_mut(body) {
            rb.set_additional_mass_properties(extra, true);
            // A piece of a split section takes a pooled body whose old colliders rapier has not
            // subtracted yet: its mass must be the chunk's now, for the split to keep momentum.
            if !carry {
                rb.recompute_mass_properties_from_colliders(colliders);
                rb.set_linvel(linvel, true);
            }
        }
        self.collider_chunks
            .insert(collider, (request.destructible, request.chunk));
        Some(ActiveChunk {
            destructible: request.destructible,
            chunk: request.chunk,
            body,
            collider,
            age_secs: 0.0,
            asleep_secs: 0.0,
            lifetime_secs: chunk.lifetime_secs,
        })
    }

    /// Returns a debris body to the pool: collider removed, body disabled.
    fn release(&mut self, physics: &mut PhysicsWorld, a: ActiveChunk) {
        self.collider_chunks.remove(&a.collider);
        physics.remove_collider(a.collider);
        self.disable(physics, a.body);
    }

    fn disable(&mut self, physics: &mut PhysicsWorld, body: RigidBodyHandle) {
        if let Some(rb) = physics.bodies.get_mut(body) {
            rb.set_linvel(rapier3d::math::Vector::ZERO, false);
            rb.set_angvel(rapier3d::math::Vector::ZERO, false);
            rb.set_enabled(false);
            self.pool.push(body);
        }
    }

    fn refresh_counts(&mut self) {
        self.stats.active = self.bodies_alive() as u32;
        self.stats.pool_free = self.pool.len() as u32;
        self.stats.sections = self.sections.len() as u32;
        self.stats.free_bodies = self.intact.values().filter(|i| i.free).count() as u32;
    }
}

/// Inserts a chunk's collider on `body`, offset by `offset` (asset space): `mass` kilograms, or
/// massless with `None` (the body then carries the chunk's mass properties).
fn insert_hull(
    physics: &mut PhysicsWorld,
    body: RigidBodyHandle,
    shape: ColliderShape,
    offset: Option<[f32; 3]>,
    mass: Option<f32>,
) -> Option<ColliderHandle> {
    let shared = shape_to_shared_shape(&shape)?;
    let mut builder = ColliderBuilder::new(shared);
    builder = match mass {
        Some(m) => builder.mass(m),
        None => builder.density(0.0),
    };
    if let Some(o) = offset {
        builder = builder.translation(to_rapier_vec3(Vec3::from_array(o)));
    }
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    Some(colliders.insert_with_parent(builder.build(), body, bodies))
}

/// Grows instance `id`'s navmesh region by the box `lo..hi`.
fn merge_region(
    regions: &mut BTreeMap<DestructibleId, ([f32; 3], [f32; 3])>,
    id: DestructibleId,
    lo: [f32; 3],
    hi: [f32; 3],
) {
    let (min, max) = regions.entry(id).or_insert((lo, hi));
    for ((m, l), (x, h)) in min.iter_mut().zip(lo).zip(max.iter_mut().zip(hi)) {
        *m = m.min(l);
        *x = x.max(h);
    }
}

/// World-space bounds of chunk `c` in the intact structure (the 8 corners of its asset-space box).
fn world_bounds(inst: &DestructibleInstance, c: usize) -> ([f32; 3], [f32; 3]) {
    let chunk = &inst.asset().chunks[c];
    let pose = inst.pose();
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for corner in 0..8u32 {
        let p = [
            if corner & 1 == 0 {
                chunk.bounds_min[0]
            } else {
                chunk.bounds_max[0]
            },
            if corner & 2 == 0 {
                chunk.bounds_min[1]
            } else {
                chunk.bounds_max[1]
            },
            if corner & 4 == 0 {
                chunk.bounds_min[2]
            } else {
                chunk.bounds_max[2]
            },
        ];
        let w = pose.transform_point(p);
        for ((l, h), v) in lo.iter_mut().zip(hi.iter_mut()).zip(w) {
            *l = l.min(v);
            *h = h.max(v);
        }
    }
    (lo, hi)
}

/// Steps `physics` once by `dt` seconds under `gravity`, without the ECS: for samples, tools and
/// headless servers that run destruction without `PhysicsPlugin`. Collision events are not
/// collected (the plugin's step does that).
#[tracing::instrument(skip_all, target = "regia_physics", name = "destruction_step_standalone")]
pub fn step_standalone(physics: &mut PhysicsWorld, dt: f32, gravity: [f32; 3]) {
    let integration = rapier3d::dynamics::IntegrationParameters {
        dt,
        ..Default::default()
    };
    let PhysicsWorld {
        pipeline,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        removed_awaiting_step,
        step_count,
        ..
    } = physics;
    // Removed colliders only matter to the plugin's event flush.
    removed_awaiting_step.clear();
    pipeline.step(
        to_rapier_vec3(Vec3::from_array(gravity)),
        &integration,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        &(),
        &(),
    );
    *step_count += 1;
}

/// Adds a fixed box collider (ground, abutments, walls around a destructible) straight into
/// rapier, for samples and tools that run without the ECS. Returns its body.
pub fn add_fixed_box(
    physics: &mut PhysicsWorld,
    centre: [f32; 3],
    half_extents: [f32; 3],
) -> RigidBodyHandle {
    let body = physics.bodies.insert(
        RigidBodyBuilder::new(RigidBodyType::Fixed)
            .translation(to_rapier_vec3(Vec3::from_array(centre)))
            .build(),
    );
    let collider = ColliderBuilder::cuboid(half_extents[0], half_extents[1], half_extents[2]).build();
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    colliders.insert_with_parent(collider, body, bodies);
    body
}

#[cfg(test)]
mod tests {
    use rapier3d::dynamics::RigidBodyBuilder;
    use rapier3d::geometry::ColliderBuilder;

    use super::super::sim::Authority;
    use super::*;

    fn warmed_sync_allocs(
        bridge: &mut DestructionPhysics,
        physics: &mut PhysicsWorld,
        world: &mut DestructionWorld,
    ) -> usize {
        let _ = bridge.sync(physics, world, 1.0 / 60.0);
        let before = crate::alloc_count::allocations();
        let _ = bridge.sync(physics, world, 1.0 / 60.0);
        crate::alloc_count::allocations() - before
    }

    #[test]
    fn idle_sync_reuses_the_active_deque() {
        let mut world = DestructionWorld::new(Authority::Server);
        world.set_budget(DestructionBudget {
            sleep_despawn_secs: 0.0,
            max_lifetime_secs: 1.0e6,
            kill_y: -1.0e6,
            ..DestructionBudget::default()
        });
        let mut physics = PhysicsWorld::default();
        let mut bridge = DestructionPhysics::default();
        let empty = warmed_sync_allocs(&mut bridge, &mut physics, &mut world);

        let body = physics.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(to_rapier_vec3(Vec3::new(0.0, 2.0, 0.0)))
                .build(),
        );
        let collider = {
            let PhysicsWorld {
                bodies, colliders, ..
            } = &mut physics;
            colliders.insert_with_parent(ColliderBuilder::cuboid(0.5, 0.5, 0.5).build(), body, bodies)
        };
        bridge.active.push_back(ActiveChunk {
            destructible: DestructibleId(1),
            chunk: 0,
            body,
            collider,
            age_secs: 0.0,
            asleep_secs: 0.0,
            lifetime_secs: Some(1.0e6),
        });
        let occupied = warmed_sync_allocs(&mut bridge, &mut physics, &mut world);
        assert_eq!(
            bridge.active().count(),
            1,
            "the chunk must stay while it cannot expire"
        );
        assert_eq!(
            occupied, empty,
            "idle sync with one chunk allocated {occupied}, empty sync allocated {empty}"
        );
        eprintln!("destruction_idle_sync_allocs {occupied} empty {empty}");

        if let Some(rb) = physics.bodies.get_mut(body) {
            rb.sleep();
        }
        world.set_budget(DestructionBudget {
            sleep_despawn_secs: 0.01,
            max_lifetime_secs: 1.0e6,
            kill_y: -1.0e6,
            ..DestructionBudget::default()
        });
        let report = bridge.sync(&mut physics, &mut world, 1.0);
        assert_eq!(report.expired, 1);
        assert!(report.despawned.contains(&(DestructibleId(1), 0)));
        assert_eq!(bridge.active().count(), 0);
    }
}
