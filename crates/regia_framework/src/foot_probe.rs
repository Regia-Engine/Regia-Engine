// SPDX-License-Identifier: MPL-2.0
//! Physics [`GroundProbe`] for [`FootPlacement`].
//!
//! `regia_animation` does not depend on physics. This module is the raycast: sensors and the
//! character body are excluded, the same query the third-person sample used to run by hand.
//!
//! The probe trait has no world borrow. Foot placement calls it while it already holds entity
//! borrows, so the raycast reads a pointer published for that schedule window and cleared at the
//! end of it (ADR-8388). The pointer is an address inside the `PhysicsWorld` resource. It is not
//! a thread-local: the schedule may run the reader on a worker.

use std::sync::atomic::{AtomicPtr, Ordering};

use regia_animation::foot_ik::{FootPlacement, FootPlacementRespawn, GroundHit, GroundProbe, GroundProbes};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use regia_physics::PhysicsWorld;
use regia_physics::queries::{QueryFilter, WorldQueries};

use crate::character::CharacterState;
use crate::damage::Died;

/// Address of the `PhysicsWorld` resource during a foot-placement window. Null outside it.
static PHYSICS_PROBE: AtomicPtr<PhysicsWorld> = AtomicPtr::new(std::ptr::null_mut());

/// Raycast probe. Install it with [`install_physics_ground_probe`] (the character plugin does).
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysicsGroundProbe;

impl GroundProbe for PhysicsGroundProbe {
    fn probe(&self, from: Vec3, down: Vec3, max_distance: f32, ignore: Entity) -> Option<GroundHit> {
        self.probe_moving(from, down, max_distance, ignore)
            .map(|(hit, _)| hit)
    }

    /// The raycast, plus the velocity of the hit body at the hit point (a moving platform).
    fn probe_moving(
        &self,
        from: Vec3,
        down: Vec3,
        max_distance: f32,
        ignore: Entity,
    ) -> Option<(GroundHit, Vec3)> {
        let ptr = PHYSICS_PROBE.load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }
        // SAFETY: ADR-8388. `publish_physics_probe` stored the address of the live
        // `PhysicsWorld` resource, and `clear_physics_probe` clears it before any later writer.
        // Foot placement does not remove that resource. This reads only; it does not write
        // through the pointer. The Update reader is an exclusive system, so nothing else holds
        // `&mut PhysicsWorld` then. The PostUpdate window is ordered after the debris writers
        // and before `apply_animation`; a writer that is not part of that order is outside the
        // window (see the ADR).
        #[allow(unsafe_code)]
        let world = unsafe { &*ptr };
        let queries = WorldQueries { world, layers: None };
        let excluded = [ignore];
        let hit = queries.raycast(
            from,
            down,
            max_distance,
            QueryFilter::new().exclude_sensors().exclude_entities(&excluded),
        )?;
        // A hit at the ray origin is not ground under the foot (the cast started on, or in,
        // a surface `probe_height` above it). A ray that started inside reports the face
        // where it leaves (ADR-8721) with that face's outward normal, along the ray (ADR-8801);
        // `GroundProbes` drops that back face by its normal.
        if hit.distance <= 1.0e-4 {
            return None;
        }
        Some((
            GroundHit {
                point: hit.point,
                normal: hit.normal,
            },
            queries.point_velocity(hit.entity, hit.point),
        ))
    }
}

/// Store the physics world's address for the foot-placement systems that run next.
pub fn publish_physics_probe(physics: Option<Res<PhysicsWorld>>) {
    let ptr = physics
        .as_deref()
        .map(|world| std::ptr::from_ref(world).cast_mut())
        .unwrap_or(std::ptr::null_mut());
    PHYSICS_PROBE.store(ptr, Ordering::Release);
}

/// Drop the published address. Call this after foot placement has finished for the stage.
pub fn clear_physics_probe() {
    PHYSICS_PROBE.store(std::ptr::null_mut(), Ordering::Release);
}

/// Install the physics probe when the world does not already have one.
///
/// A host or a test that inserted its own [`GroundProbes`] (an analytic plane, a fake) keeps it.
pub fn install_physics_ground_probe(world: &mut World) {
    if world.get_resource::<GroundProbes>().is_none() {
        world.insert_resource(GroundProbes::new(PhysicsGroundProbe));
    }
}

/// Copy the character's grounded flag onto its rig's [`FootPlacement`].
///
/// A placement with no character movement component keeps the flag the host set (`None` stays
/// always active; `Some(false)` stays in the air). The character is `placement.character`, or
/// the rig entity itself when that is unset.
pub fn sync_foot_grounded(
    characters: Query<&CharacterState>,
    mut placements: Query<(Entity, &mut FootPlacement)>,
) {
    for (entity, mut placement) in &mut placements {
        let character = placement.character.unwrap_or(entity);
        if let Ok(state) = characters.get(character) {
            placement.grounded = Some(state.grounded);
        }
    }
}

/// When `Died` is removed (a respawn), mark the rig so foot placement clears its pins this frame.
///
/// Exclusive: the marker has to be on the entity before `prepare_foot_placement` in the same
/// `Update`, and a deferred command would not be visible until the next stage.
pub fn foot_placement_on_respawn(world: &mut World) {
    let removed: Vec<Entity> = world.removed::<Died>().collect();
    if removed.is_empty() {
        return;
    }
    let mut mark = Vec::new();
    let mut query = world.query::<(Entity, &FootPlacement)>();
    for (rig, placement) in query.iter(world) {
        let character = placement.character.unwrap_or(rig);
        if removed.contains(&character) || removed.contains(&rig) {
            mark.push(rig);
        }
    }
    for rig in mark {
        if let Ok(mut entity) = world.get_entity_mut(rig) {
            entity.insert(FootPlacementRespawn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flat {
        height: f32,
    }

    impl GroundProbe for Flat {
        fn probe(&self, from: Vec3, _down: Vec3, _max_distance: f32, _ignore: Entity) -> Option<GroundHit> {
            Some(GroundHit {
                point: Vec3::new(from.x, self.height, from.z),
                normal: Vec3::Y,
            })
        }
    }

    #[test]
    fn install_leaves_an_existing_probe() {
        let mut world = World::new();
        world.insert_resource(GroundProbes::new(Flat { height: 3.0 }));
        install_physics_ground_probe(&mut world);
        let hit = world.resource::<GroundProbes>().probe(
            Vec3::new(1.0, 10.0, 2.0),
            Vec3::NEG_Y,
            20.0,
            Entity::PLACEHOLDER,
        );
        assert_eq!(hit.map(|hit| hit.point.y), Some(3.0));
    }

    #[test]
    fn removing_died_marks_the_rig() {
        let mut world = World::new();
        let character = world.spawn(Died).id();
        let mut placement = FootPlacement::default();
        placement.character = Some(character);
        let rig = world.spawn(placement).id();
        world.entity_mut(character).remove::<Died>();
        foot_placement_on_respawn(&mut world);
        assert!(world.get::<FootPlacementRespawn>(rig).is_some());
    }
}
