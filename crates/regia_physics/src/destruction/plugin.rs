// SPDX-License-Identifier: MPL-2.0
//! ECS integration: the [`Destruction`] resource, [`DestructionPlugin`] and the messages games
//! hook sound, VFX, networking and navmesh rebuilds to.
//!
//! Each `FixedUpdate`, before the physics step, [`follow_root_transforms`] moves structures whose
//! root entity moved, then [`step_destruction`] runs the break model one tick
//! ([`DestructionWorld::step`]), syncs rapier ([`DestructionPhysics::sync`]), spawns an entity per
//! new debris body ([`DestructionChunk`] + `Transform`) and despawns the ones that went back to
//! the pool, and writes [`DestructionBroke`], [`DestructionFxMessage`] and [`NavmeshInvalidated`].
//! After the step, [`write_back_destruction`] copies debris poses (single chunks and falling
//! sections) to their `Transform`s and [`write_back_free_roots`] moves free bodies' roots.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_ecs::ecs::message::Messages;
use regia_scene::Transform;

use super::asset::DestructibleAsset;
use super::bridge::{BridgeReport, DestructionPhysics, NavInvalidation};
use super::impact::{DestructionImpact, convert_impacts};
use super::sim::{
    Authority, BreakEvent, DestructibleId, DestructiblePose, DestructionError, DestructionFx,
    DestructionSnapshot, DestructionWorld, InstanceSnapshot, StepReport,
};
use crate::plugin::PhysicsStep;
use crate::world::{Gravity, PhysicsWorld};

/// A break was decided (or replayed): the event a server sends to its clients.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct DestructionBroke(pub BreakEvent);

/// A sound/VFX moment: play the `fx_tag`'s effect at `position`.
#[derive(Message, Debug, Clone, PartialEq)]
pub struct DestructionFxMessage(pub DestructionFx);

/// Chunks left a structure: rebuild the navmesh tiles under this box.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct NavmeshInvalidated(pub NavInvalidation);

/// On the entity of each destructible's intact structure (raycasts against its colliders report
/// this entity).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestructibleRoot(pub DestructibleId);

/// On the entity of each debris chunk.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestructionChunk {
    /// The instance.
    pub destructible: DestructibleId,
    /// Chunk index.
    pub chunk: u32,
}

/// What the last fixed tick did (for tools and the agent surface).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickSummary {
    /// The tick.
    pub tick: u64,
    /// Break events.
    pub events: usize,
    /// Hits applied.
    pub hits: u32,
    /// Hits that reached nothing.
    pub misses: u32,
    /// Breaks held back by the per-tick cap.
    pub deferred_breaks: u32,
    /// Debris bodies spawned.
    pub spawned: usize,
    /// Debris bodies returned.
    pub despawned: usize,
    /// Intact colliders removed.
    pub colliders_removed: u32,
    /// Navmesh regions reported.
    pub nav_regions: usize,
}

/// Every destructible of the app: the break model and its rapier side.
#[derive(Resource, Debug, Default)]
pub struct Destruction {
    /// The deterministic break model.
    pub world: DestructionWorld,
    /// The rapier side.
    pub physics: DestructionPhysics,
    roots: BTreeMap<DestructibleId, Entity>,
    chunk_entities: HashMap<(DestructibleId, u32), Entity>,
    /// Entities of instances a snapshot replaced; despawned on the next tick.
    stale_entities: Vec<Entity>,
    last: TickSummary,
    totals: MessageTotals,
}

/// Messages written since the resource was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageTotals {
    /// [`DestructionBroke`] messages.
    pub breaks: u64,
    /// [`DestructionFxMessage`] messages.
    pub fx: u64,
    /// [`NavmeshInvalidated`] messages.
    pub nav: u64,
}

impl Destruction {
    /// An empty resource with the given authority.
    #[must_use]
    pub fn new(authority: Authority) -> Self {
        Self {
            world: DestructionWorld::new(authority),
            ..Self::default()
        }
    }

    /// Places a destructible and builds its intact colliders (its root entity is spawned on the
    /// next tick).
    ///
    /// # Errors
    /// See [`DestructionWorld::define`].
    pub fn define(
        &mut self,
        physics: &mut PhysicsWorld,
        asset: Arc<DestructibleAsset>,
        pose: DestructiblePose,
        seed: u64,
    ) -> Result<DestructibleId, DestructionError> {
        let id = self.world.define(asset, pose, seed)?;
        self.physics.attach(physics, &self.world, id)?;
        Ok(id)
    }

    /// Removes a destructible, its colliders and its debris (entities go on the next tick).
    pub fn remove(&mut self, physics: &mut PhysicsWorld, id: DestructibleId) -> bool {
        self.physics.detach(physics, id);
        self.world.remove(id)
    }

    /// Removes a destructible from a replica, which may run without rapier: the [`Self::remove`]
    /// of a session that a server drives.
    pub fn remove_replicated(&mut self, physics: Option<&mut PhysicsWorld>, id: DestructibleId) -> bool {
        if let Some(physics) = physics {
            self.physics.detach(physics, id);
        }
        self.world.remove(id)
    }

    /// Replaces a replica's world with a server's `snapshot` ([`DestructionWorld::restore`]). The
    /// old instances leave rapier here, their root and debris entities go on the next tick, and the
    /// next tick builds the new instances' colliders and roots: chunks the snapshot says are
    /// detached get neither a collider nor a body.
    ///
    /// # Errors
    /// See [`DestructionWorld::restore`]; nothing changes on an error.
    pub fn restore(
        &mut self,
        physics: Option<&mut PhysicsWorld>,
        snapshot: &DestructionSnapshot,
        assets: &dyn Fn(DestructibleId) -> Option<Arc<DestructibleAsset>>,
    ) -> Result<(), DestructionError> {
        let old: Vec<DestructibleId> = self.world.instances().map(|(id, _)| id).collect();
        self.world.restore(snapshot, assets)?;
        if let Some(physics) = physics {
            for id in old {
                self.physics.detach(physics, id);
            }
        }
        self.stale_entities.extend(self.roots.values().copied());
        self.roots.clear();
        self.stale_entities
            .extend(self.chunk_entities.drain().map(|(_, e)| e));
        // Map order must not decide the despawn order: it feeds which ids get reused.
        self.stale_entities.sort_unstable();
        Ok(())
    }

    /// Adds one instance to a replica in a server's state ([`DestructionWorld::restore_instance`]):
    /// a structure that became relevant to this client (ADR-2305). Its colliders and root entity
    /// come on the next tick.
    ///
    /// # Errors
    /// See [`DestructionWorld::restore_instance`]; nothing changes on an error.
    pub fn restore_instance(
        &mut self,
        state: &InstanceSnapshot,
        asset: Arc<DestructibleAsset>,
    ) -> Result<(), DestructionError> {
        self.world.restore_instance(state, asset)
    }

    /// What the last fixed tick did.
    #[must_use]
    pub fn last_tick(&self) -> &TickSummary {
        &self.last
    }

    /// Messages written since the resource was created.
    #[must_use]
    pub fn message_totals(&self) -> MessageTotals {
        self.totals
    }

    /// The root entity of an instance, once spawned.
    #[must_use]
    pub fn root_entity(&self, id: DestructibleId) -> Option<Entity> {
        self.roots.get(&id).copied()
    }

    /// The entity of a chunk's debris body, if it has one.
    #[must_use]
    pub fn chunk_entity(&self, id: DestructibleId, chunk: u32) -> Option<Entity> {
        self.chunk_entities.get(&(id, chunk)).copied()
    }
}

/// Adds [`Destruction`], the messages and the systems ([`follow_root_transforms`],
/// `copy_section_gravity` and [`step_destruction`] before the physics step,
/// [`write_back_destruction`] after it), plus [`DestructionImpact`] and [`convert_impacts`] after
/// the physics step (impact damage, ADR-3272: nothing happens until a structure is enabled), and
/// [`super::source::define_destructible_sources`] in `PreUpdate` for level-placed destructibles.
#[derive(Debug, Clone, Copy, Default)]
pub struct DestructionPlugin {
    /// Who decides breaks ([`Authority::Server`] for single player).
    pub authority: Authority,
}

impl Plugin for DestructionPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<Destruction>() {
            app.insert_resource(Destruction::new(self.authority));
        }
        if !app.world().contains_resource::<DestructionImpact>() {
            app.insert_resource(DestructionImpact::default());
        }
        app.add_message::<DestructionBroke>()
            .add_message::<DestructionFxMessage>()
            .add_message::<NavmeshInvalidated>()
            .add_systems(
                FixedUpdate,
                (follow_root_transforms, copy_section_gravity, step_destruction)
                    .chain()
                    .before(PhysicsStep),
            )
            .add_systems(
                FixedUpdate,
                (write_back_destruction, write_back_free_roots).after(PhysicsStep),
            )
            .add_systems(FixedUpdate, convert_impacts.after(PhysicsStep))
            // Level-placed destructibles (ADR-9750): outside the fixed tick, before it.
            .register_type::<super::source::DestructibleSource>()
            .add_systems(PreUpdate, super::source::define_destructible_sources);
    }
}

/// Moves destructibles whose root entity's `Transform` changed (a structure on a vehicle: copy the
/// vehicle's pose to the root each fixed tick). The root is a top-level entity, so its `Transform`
/// is its world pose; scale is ignored.
pub fn follow_root_transforms(
    mut destruction: ResMut<Destruction>,
    mut physics: Option<ResMut<PhysicsWorld>>,
    roots: Query<(&DestructibleRoot, &Transform), Changed<Transform>>,
) {
    let _span = tracing::debug_span!("follow_root_transforms").entered();
    let d = &mut *destruction;
    for (root, t) in &roots {
        let pose = DestructiblePose {
            translation: t.translation.to_array(),
            rotation: t.rotation.to_array(),
        };
        let Some(current) = d.world.instance(root.0).map(|i| i.pose()) else {
            continue;
        };
        // The plugin's own spawn of the root changes nothing.
        if current == pose || d.world.set_pose(root.0, pose).is_err() {
            continue;
        }
        if let Some(physics) = physics.as_deref_mut() {
            // The id comes from the world, so the follow cannot fail on it.
            let _ = d.physics.follow_pose(physics, &d.world, root.0);
        }
    }
}

/// Copies the gravity the previous physics step stored onto the section impact test.
///
/// `impact_sections` subtracts that vector from the velocity change. The step stored it on
/// `Gravity` (level gravity, else `PhysicsSettings`, the last finite value when the request was
/// not finite). A world with no physics plugin has no `Gravity` resource, and the Earth fallback
/// stays. Runs before [`step_destruction`], so the copy is in place when sections are tested.
fn copy_section_gravity(gravity: Option<Res<Gravity>>, mut destruction: ResMut<Destruction>) {
    let Some(gravity) = gravity.as_deref() else {
        return;
    };
    let g = gravity.0;
    destruction.physics.set_gravity([g.x, g.y, g.z]);
}

/// One fixed tick of destruction (before the physics step): decide, sync rapier, spawn and
/// despawn debris entities, write the messages.
pub fn step_destruction(
    fixed: Res<FixedTime>,
    mut destruction: ResMut<Destruction>,
    physics: Option<ResMut<PhysicsWorld>>,
    mut commands: Commands,
    mut broke: ResMut<Messages<DestructionBroke>>,
    mut fx: ResMut<Messages<DestructionFxMessage>>,
    mut nav: ResMut<Messages<NavmeshInvalidated>>,
) {
    let _span = tracing::debug_span!(target: "regia_physics", "step_destruction").entered();
    let dt = fixed.step().as_secs_f32();
    let d = &mut *destruction;
    for e in d.stale_entities.drain(..) {
        commands.entity(e).try_despawn();
    }
    let report: StepReport = d.world.step(dt);
    let mut bridge = BridgeReport::default();
    if let Some(mut physics) = physics {
        let physics = &mut *physics;
        // Late-defined instances (a replica defining after the fact) get their colliders here.
        let missing: Vec<DestructibleId> = d
            .world
            .instances()
            .map(|(id, _)| id)
            .filter(|id| d.physics.intact_body(*id).is_none())
            .collect();
        for id in missing {
            // The id comes from the world, so attach cannot fail on it.
            let _ = d.physics.attach(physics, &d.world, id);
        }
        bridge = d.physics.sync(physics, &mut d.world, dt);
        // Root entities for new instances; their intact colliders report the root in queries.
        for (id, inst) in d.world.instances() {
            if d.roots.contains_key(&id) {
                continue;
            }
            let pose = inst.pose();
            let root = commands
                .spawn((
                    DestructibleRoot(id),
                    Transform {
                        translation: regia_core::math::Vec3::from_array(pose.translation),
                        rotation: regia_core::math::Quat::from_array(pose.rotation),
                        scale: regia_core::math::Vec3::ONE,
                    },
                ))
                .id();
            for chunk in 0..inst.asset().chunks.len() as u32 {
                if let Some(h) = d.physics.intact_collider(id, chunk) {
                    physics.register_collider(h, root);
                }
            }
            d.roots.insert(id, root);
        }
        // `try_despawn`: a game may have despawned a debris entity itself.
        for (id, chunk) in &bridge.despawned {
            if let Some(e) = d.chunk_entities.remove(&(*id, *chunk)) {
                commands.entity(e).try_despawn();
            }
        }
        // In spawn order; a chunk evicted again in the same tick has no collider and no entity.
        for &(id, chunk) in &bridge.spawned {
            let Some(collider) = d.physics.debris_collider(id, chunk) else {
                continue;
            };
            let (translation, rotation) = d.physics.debris_pose(physics, id, chunk).unwrap_or_default();
            let e = commands
                .spawn((
                    DestructionChunk {
                        destructible: id,
                        chunk,
                    },
                    Transform {
                        translation,
                        rotation,
                        scale: regia_core::math::Vec3::ONE,
                    },
                ))
                .id();
            physics.register_collider(collider, e);
            d.chunk_entities.insert((id, chunk), e);
        }
        // A split section's chunks keep their entities and get new colliders.
        for split in &bridge.splits {
            for &chunk in &split.chunks {
                if let (Some(e), Some(collider)) = (
                    d.chunk_entities.get(&(split.destructible, chunk)),
                    d.physics.debris_collider(split.destructible, chunk),
                ) {
                    physics.register_collider(collider, *e);
                }
            }
        }
    }
    // Roots of removed instances.
    let gone: Vec<DestructibleId> = d
        .roots
        .keys()
        .copied()
        .filter(|id| d.world.instance(*id).is_none())
        .collect();
    for id in gone {
        if let Some(e) = d.roots.remove(&id) {
            commands.entity(e).try_despawn();
        }
        let mut chunks: Vec<u32> = d
            .chunk_entities
            .keys()
            .filter(|k| k.0 == id)
            .map(|k| k.1)
            .collect();
        // The map's order differs per process, and a despawn order decides which ids later spawns
        // reuse.
        chunks.sort_unstable();
        for k in chunks.into_iter().map(|chunk| (id, chunk)) {
            if let Some(e) = d.chunk_entities.remove(&k) {
                commands.entity(e).try_despawn();
            }
        }
    }
    d.last = TickSummary {
        tick: report.tick,
        events: report.events.len(),
        hits: report.hits,
        misses: report.misses,
        deferred_breaks: report.deferred_breaks,
        spawned: bridge.spawned.len(),
        despawned: bridge.despawned.len(),
        colliders_removed: bridge.colliders_removed,
        nav_regions: bridge.nav.len(),
    };
    d.totals.breaks += report.events.len() as u64;
    d.totals.fx += report.fx.len() as u64;
    d.totals.nav += bridge.nav.len() as u64;
    for e in report.events {
        broke.write(DestructionBroke(e));
    }
    for f in report.fx {
        fx.write(DestructionFxMessage(f));
    }
    for n in bridge.nav {
        nav.write(NavmeshInvalidated(n));
    }
}

/// Copies debris poses to their entities' `Transform` (after the physics step).
pub fn write_back_destruction(
    destruction: Res<Destruction>,
    physics: Option<Res<PhysicsWorld>>,
    mut chunks: Query<&mut Transform, With<DestructionChunk>>,
) {
    let _span = tracing::debug_span!(target: "regia_physics", "write_back_destruction").entered();
    let Some(physics) = physics else {
        return;
    };
    for a in destruction.physics.active() {
        let Some(e) = destruction.chunk_entity(a.destructible, a.chunk) else {
            continue;
        };
        let (Ok(mut t), Some(rb)) = (chunks.get_mut(e), physics.bodies.get(a.body)) else {
            continue;
        };
        let (translation, rotation) = crate::convert::from_rapier_pose(rb.position());
        t.translation = translation;
        t.rotation = rotation;
    }
    // Chunks of falling sections (ADR-2734).
    for sec in destruction.physics.sections() {
        for &chunk in &sec.chunks {
            let Some(e) = destruction.chunk_entity(sec.destructible, chunk) else {
                continue;
            };
            let (Ok(mut t), Some((translation, rotation))) = (
                chunks.get_mut(e),
                destruction.physics.debris_pose(&physics, sec.destructible, chunk),
            ) else {
                continue;
            };
            t.translation = translation;
            t.rotation = rotation;
        }
    }
}

/// Copies free-body structures' poses to their root entities' `Transform` (after the physics
/// step, ADR-2734): a free object's root moves with it.
pub fn write_back_free_roots(
    destruction: Res<Destruction>,
    physics: Option<Res<PhysicsWorld>>,
    mut roots: Query<(&DestructibleRoot, &mut Transform), Without<DestructionChunk>>,
) {
    let Some(physics) = physics else {
        return;
    };
    if destruction.physics.stats().free_bodies == 0 {
        return;
    }
    let _span = tracing::debug_span!(target: "regia_physics", "write_back_free_roots").entered();
    let poses: BTreeMap<DestructibleId, (regia_core::math::Vec3, regia_core::math::Quat)> = destruction
        .physics
        .free_body_poses(&physics)
        .map(|(id, t, q)| (id, (t, q)))
        .collect();
    for (root, mut t) in &mut roots {
        if let Some((translation, rotation)) = poses.get(&root.0) {
            t.translation = *translation;
            t.rotation = *rotation;
        }
    }
}
