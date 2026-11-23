// SPDX-License-Identifier: MPL-2.0
//! Editor-visible warning for a physics body parented under a non-physics parent
//! (spec 15 § 1: rapier writes a body's world pose back into its `Transform`, so a
//! parented body's transform is local to a parent the simulation knows nothing about;
//! bodies are recommended as hierarchy roots).
//!
//! The check runs wherever the [`crate::PhysicsPlugin`] runs: the editor's edit world has no
//! physics, so this is a play-world-only check by construction. Only `Dynamic` bodies are
//! checked (spec 15 § 1 step 3 writes back `Dynamic` poses only); each offender is logged once
//! and counted; `regia_agent`'s `physics.parented_bodies` reads the count.

use std::collections::HashSet;

use regia_ecs::ecs::entity::Entities;
use regia_ecs::prelude::*;

use crate::{BodyKind, RigidBody};

/// One found body under a non-physics parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParentedBodyWarning {
    /// The entity carrying the `RigidBody`.
    pub entity: Entity,
    /// Its parent, which has no `RigidBody`.
    pub parent: Entity,
}

/// Counted, de-duplicated findings of [`warn_parented_bodies`].
#[derive(Resource, Default, Debug)]
pub struct ParentedBodyWarnings {
    /// Every distinct body found under a non-physics parent, in the order found.
    pub entries: Vec<ParentedBodyWarning>,
    warned: HashSet<Entity>,
}

impl ParentedBodyWarnings {
    /// How many distinct parented bodies the run has found.
    #[must_use]
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Drops every finding (a new play session starts clean).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.warned.clear();
    }

    fn record(&mut self, entity: Entity, parent: Entity) {
        if self.warned.insert(entity) {
            self.entries.push(ParentedBodyWarning { entity, parent });
        }
    }
}

/// Finds `Dynamic` [`RigidBody`] entities whose `ChildOf` parent has no `RigidBody` and records
/// each once (with a `tracing::warn!` for the editor log). Spec 15 § 1 step 3 writes a body's
/// world pose back into its `Transform` only for `Dynamic` bodies, so only those fight the
/// hierarchy (a `Static` or `Kinematic` body under a level group or prefab root is the normal,
/// supported case — `step.rs` `world_pose`/`local_pose`). The spec recommends bodies as
/// hierarchy roots. Findings whose body has despawned are pruned every pass, so a session that
/// spawns projectiles under a pivot does not grow the collections for its whole run.
pub fn warn_parented_bodies(
    mut warnings: ResMut<ParentedBodyWarnings>,
    entities: &Entities,
    children: Query<(Entity, &ChildOf, &RigidBody)>,
    bodies: Query<(), With<RigidBody>>,
) {
    warnings.entries.retain(|found| entities.contains(found.entity));
    warnings.warned = warnings.entries.iter().map(|found| found.entity).collect();
    for (entity, child_of, body) in &children {
        if body.kind != BodyKind::Dynamic {
            continue;
        }
        let parent = child_of.parent();
        if bodies.get(parent).is_err() {
            let fresh = !warnings.warned.contains(&entity);
            warnings.record(entity, parent);
            if fresh {
                tracing::warn!(
                    target: "regia_physics",
                    %entity,
                    %parent,
                    "dynamic physics body parented under a non-physics parent: the transform is \
                     local while rapier writes world poses back (spec 15 § 1); make the body a \
                     hierarchy root"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Collider;
    use regia_app::prelude::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_systems(PostUpdate, warn_parented_bodies);
        app.init_resource::<ParentedBodyWarnings>();
        app
    }

    #[test]
    fn a_body_under_a_body_is_fine_and_a_root_body_is_fine() {
        let mut app = app();
        let root = app.world_mut().spawn(RigidBody::default()).id();
        app.world_mut().spawn((RigidBody::default(), ChildOf(root)));
        app.update();
        let warnings = app.world().resource::<ParentedBodyWarnings>();
        assert_eq!(warnings.count(), 0, "{warnings:?}");
    }

    #[test]
    fn a_body_under_a_body_without_physics_warns_once() {
        let mut app = app();
        let parent = app.world_mut().spawn_empty().id();
        let body = app
            .world_mut()
            .spawn((RigidBody::default(), ChildOf(parent)))
            .id();
        app.update();
        app.update();
        let warnings = app.world().resource::<ParentedBodyWarnings>();
        assert_eq!(warnings.count(), 1, "{warnings:?}");
        assert_eq!(warnings.entries[0], ParentedBodyWarning { entity: body, parent });
    }

    #[test]
    fn a_fixed_or_kinematic_body_under_a_non_physics_parent_is_the_supported_case() {
        // Spec 15 § 1 step 3 writes back only `Dynamic` poses; a `Static` or `Kinematic` body
        // under a level group or prefab root is the normal, supported case (step.rs
        // world_pose/local_pose) and must not warn — a level of static props under group nodes
        // would otherwise log one warning per prop.
        let mut app = app();
        let parent = app.world_mut().spawn_empty().id();
        app.world_mut().spawn((
            RigidBody {
                kind: crate::BodyKind::Static,
                ..RigidBody::default()
            },
            ChildOf(parent),
        ));
        app.world_mut().spawn((
            RigidBody {
                kind: crate::BodyKind::Kinematic,
                ..RigidBody::default()
            },
            ChildOf(parent),
        ));
        app.update();
        assert_eq!(app.world().resource::<ParentedBodyWarnings>().count(), 0);
    }

    #[test]
    fn a_despawned_body_is_pruned_from_the_warnings() {
        // A game that spawns dynamic projectiles under a non-physics pivot must not grow the
        // findings for the whole play session: once the body despawns, its finding is dropped.
        let mut app = app();
        let parent = app.world_mut().spawn_empty().id();
        let first = app
            .world_mut()
            .spawn((RigidBody::default(), ChildOf(parent)))
            .id();
        app.update();
        assert_eq!(app.world().resource::<ParentedBodyWarnings>().count(), 1);
        app.world_mut().despawn(first);
        app.update();
        assert_eq!(
            app.world().resource::<ParentedBodyWarnings>().count(),
            0,
            "despawned entries must be pruned"
        );
        // A fresh body under the same parent is a new finding (and warns again).
        app.world_mut().spawn((RigidBody::default(), ChildOf(parent)));
        app.update();
        assert_eq!(app.world().resource::<ParentedBodyWarnings>().count(), 1);
    }

    #[test]
    fn a_collider_child_under_a_body_is_not_a_body() {
        // Compound colliders attach to the nearest ancestor body (spec 15 § 2); only a
        // RigidBody under a non-physics parent is the warned case.
        let mut app = app();
        let parent = app.world_mut().spawn(RigidBody::default()).id();
        app.world_mut().spawn((Collider::default(), ChildOf(parent)));
        app.update();
        assert_eq!(app.world().resource::<ParentedBodyWarnings>().count(), 0);
    }

    #[test]
    fn clear_resets_the_count() {
        let mut app = app();
        let parent = app.world_mut().spawn_empty().id();
        app.world_mut().spawn((RigidBody::default(), ChildOf(parent)));
        app.update();
        app.world_mut().resource_mut::<ParentedBodyWarnings>().clear();
        assert_eq!(app.world().resource::<ParentedBodyWarnings>().count(), 0);
    }
}
