// SPDX-License-Identifier: MPL-2.0
//! Parent/child operations on the bevy_ecs `ChildOf`/`Children` relationships
//! (`docs/specs/14-scene-and-framework.md` A1), with Regia Engine-style helpers:
//! `set_parent`, `remove_parent`, `keep_world_transform` reparenting and `despawn_recursive`
//! semantics.
//!
//! The hierarchy itself is bevy_ecs relationships: `commands.entity(e).insert(ChildOf(p))`
//! maintains both sides. These wrappers exist for discoverability, for the world-transform
//! preservation math, and for safety: bevy refuses an entity as its own parent but not a longer
//! cycle (parenting an entity under its own descendant), which would detach the whole loop from
//! every root. The `set_parent*` helpers check [`would_create_cycle`] and refuse such a request
//! with a warning instead.

use std::collections::HashSet;

use regia_ecs::prelude::*;

use crate::transform::GlobalTransform;

/// The local `Transform` `entity` would need under `new_parent` to keep its current world
/// transform. `None` means "become a root". Returns the identity transform when the entity has no
/// [`GlobalTransform`] yet (nothing to preserve).
///
/// Only entities inside the transform hierarchy (with a `Transform` or a `ChildOf` link) pass a
/// frame down; any other parent contributes the identity, as in propagation, even if it still
/// carries a `GlobalTransform` from before it left the hierarchy.
pub fn local_under_parent(
    world: &World,
    entity: Entity,
    new_parent: Option<Entity>,
) -> crate::transform::Transform {
    let Some(current) = world_frame(world, entity) else {
        return crate::transform::Transform::IDENTITY;
    };
    let parent_global = new_parent
        .and_then(|p| world_frame(world, p))
        .unwrap_or(GlobalTransform::IDENTITY);
    // The inverse is non-finite exactly when the parent has no usable inverse (a zero scale).
    // `determinant().abs() > f32::EPSILON` is the wrong test: a uniform scale of 0.001 has
    // determinant 1e-9 and is invertible, and keep-world must divide by that scale.
    let inverse = parent_global.0.inverse();
    let keep_local = || {
        world
            .get::<crate::transform::Transform>(entity)
            .copied()
            .unwrap_or(crate::transform::Transform::IDENTITY)
    };
    if !inverse.matrix3.is_finite() || !inverse.translation.is_finite() {
        return keep_local();
    }
    let local = crate::transform::Transform::from_affine(inverse * current.0);
    if local.translation.is_finite() && local.scale.is_finite() && local.rotation.is_finite() {
        local
    } else {
        keep_local()
    }
}

/// The frame `entity` passes to its children: its `GlobalTransform` while it is part of the
/// transform hierarchy, `None` otherwise.
fn world_frame(world: &World, entity: Entity) -> Option<GlobalTransform> {
    let in_hierarchy =
        world.get::<crate::transform::Transform>(entity).is_some() || world.get::<ChildOf>(entity).is_some();
    if in_hierarchy {
        world.get::<GlobalTransform>(entity).copied()
    } else {
        None
    }
}

/// True when parenting `child` under `new_parent` would close a `ChildOf` loop: `new_parent` is
/// `child` itself or one of its descendants. Walks up from `new_parent`, so the cost is the depth
/// of `new_parent`; an already-malformed ancestry (a loop above `new_parent`) also answers `true`.
pub fn would_create_cycle(world: &World, child: Entity, new_parent: Entity) -> bool {
    let mut seen: HashSet<Entity> = HashSet::new();
    let mut cursor = new_parent;
    loop {
        if cursor == child || !seen.insert(cursor) {
            return true;
        }
        match world.get::<ChildOf>(cursor) {
            Some(link) => cursor = link.parent(),
            None => return false,
        }
    }
}

/// Despawns `root` and its whole `Children` subtree without recursion; returns whether `root`
/// existed.
///
/// `World::despawn` follows the linked `Children` relationship recursively, a set of stack frames
/// per level, so a deep chain overflows a thread's stack (a 5,000-link chain on a 2 MiB Windows
/// test thread, the v0.2.8 Windows pass). This walk collects the subtree in pre-order with an
/// explicit stack and despawns it in reverse, so every entity is a leaf when it goes and no
/// despawn recurses. Siblings go last to first, which is where the parent's `Children` list
/// looks first when it drops them, so a wide parent costs no quadratic search either. An entity
/// already visited is skipped, so a malformed hierarchy (a `ChildOf` loop) still terminates.
pub fn despawn_tree(world: &mut World, root: Entity) -> bool {
    if world.get_entity(root).is_err() {
        return false;
    }
    let mut seen: HashSet<Entity> = HashSet::new();
    let mut order: Vec<Entity> = Vec::new();
    let mut stack = vec![root];
    while let Some(entity) = stack.pop() {
        if !seen.insert(entity) {
            continue;
        }
        order.push(entity);
        if let Some(children) = world.get::<Children>(entity) {
            stack.extend(children.iter().rev());
        }
    }
    for entity in order.into_iter().rev() {
        // A hook of another relationship may already have taken it.
        if let Ok(entity_mut) = world.get_entity_mut(entity) {
            entity_mut.despawn();
        }
    }
    true
}

/// Entity-command helpers for the transform hierarchy: `set_parent` / `remove_parent` (plain) and
/// the `*_keep_world_transform` variants that recompute the local transform so the entity does not
/// visually move.
///
/// ```
/// use regia_app::prelude::*;
/// use regia_scene::prelude::*;
/// # fn demo(mut commands: Commands, parent: Entity, child: Entity) {
/// commands.entity(child)
///     .set_parent_keep_world_transform(parent);
/// # }
/// ```
pub trait HierarchyCommands {
    /// Parents the entity to `parent` (local `Transform` unchanged ⇒ world transform changes).
    /// A missing `parent`, or one that is the entity or its descendant (a cycle), is refused with
    /// a warning and the hierarchy is left unchanged.
    fn set_parent(&mut self, parent: Entity) -> &mut Self;

    /// Parents the entity to `parent`, adjusting its local `Transform` so its world transform is
    /// preserved. Refuses a missing parent or a cycle like [`HierarchyCommands::set_parent`].
    fn set_parent_keep_world_transform(&mut self, parent: Entity) -> &mut Self;

    /// Detaches the entity from its parent (local `Transform` unchanged ⇒ world transform changes).
    fn remove_parent(&mut self) -> &mut Self;

    /// Detaches the entity from its parent, writing its world transform into the local
    /// `Transform` so it does not visually move.
    fn remove_parent_keep_world_transform(&mut self) -> &mut Self;

    /// Despawns the entity and its whole descendant subtree. In bevy_ecs 0.20 relationships,
    /// `despawn` already despawns `Children` descendants (linked despawn), but recursively; this
    /// command runs [`despawn_tree`] instead, so a deep hierarchy cannot overflow the stack. A
    /// missing entity is a no-op.
    fn despawn_recursive(&mut self) -> &mut Self;
}

impl HierarchyCommands for EntityCommands<'_> {
    fn set_parent(&mut self, parent: Entity) -> &mut Self {
        let child = self.id();
        self.commands().queue(move |world: &mut World| {
            if world.get_entity(parent).is_err() {
                tracing::warn!(target: "regia_scene", "set_parent: parent {parent:?} does not exist");
                return;
            }
            if would_create_cycle(world, child, parent) {
                tracing::warn!(target: "regia_scene", "set_parent: {parent:?} is {child:?} or its descendant; refusing a hierarchy cycle");
                return;
            }
            if let Ok(mut entity) = world.get_entity_mut(child) {
                entity.insert(ChildOf(parent));
            }
        });
        self
    }

    fn set_parent_keep_world_transform(&mut self, parent: Entity) -> &mut Self {
        let child = self.id();
        self.commands().queue(move |world: &mut World| {
            if world.get_entity(parent).is_err() {
                tracing::warn!(target: "regia_scene", "set_parent_keep_world_transform: parent {parent:?} does not exist");
                return;
            }
            if would_create_cycle(world, child, parent) {
                tracing::warn!(target: "regia_scene", "set_parent_keep_world_transform: {parent:?} is {child:?} or its descendant; refusing a hierarchy cycle");
                return;
            }
            let local = local_under_parent(world, child, Some(parent));
            let Ok(mut entity) = world.get_entity_mut(child) else {
                tracing::warn!(target: "regia_scene", "set_parent_keep_world_transform: child {child:?} was despawned");
                return;
            };
            entity.insert(local);
            entity.insert(ChildOf(parent));
        });
        self
    }

    fn remove_parent(&mut self) -> &mut Self {
        self.remove::<ChildOf>()
    }

    fn remove_parent_keep_world_transform(&mut self) -> &mut Self {
        let child = self.id();
        self.commands().queue(move |world: &mut World| {
            let local = local_under_parent(world, child, None);
            let Ok(mut entity) = world.get_entity_mut(child) else {
                return;
            };
            entity.insert(local);
            entity.remove::<ChildOf>();
        });
        self
    }

    fn despawn_recursive(&mut self) -> &mut Self {
        let root = self.id();
        self.commands().queue(move |world: &mut World| {
            if !despawn_tree(world, root) {
                tracing::debug!(target: "regia_scene", "despawn_recursive: {root:?} was already despawned");
            }
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::propagate::propagate_transforms;
    use crate::transform::Transform;
    use regia_core::math::Vec3;
    use regia_ecs::ecs::world::CommandQueue;

    #[test]
    fn keep_world_reparent_under_a_zero_scale_parent_stays_finite() {
        let mut world = World::new();
        let parent = world
            .spawn(Transform {
                scale: Vec3::new(0.0, 1.0, 1.0),
                ..Transform::IDENTITY
            })
            .id();
        let child = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)))
            .id();
        propagate_transforms(&mut world);
        let local = local_under_parent(&world, child, Some(parent));
        assert!(local.translation.is_finite() && local.scale.is_finite());
        assert_eq!(local.translation, Vec3::new(1.0, 2.0, 3.0));
    }

    /// Scale 0.001 is a real parent (millimetre content). `det.abs() > f32::EPSILON` is false
    /// for it (`1e-9`), so keep-world reparent used to copy the old local pose instead of
    /// dividing the world pose by the parent scale.
    #[test]
    fn keep_world_reparent_under_a_millimetre_scale_preserves_pose() {
        let mut world = World::new();
        let parent = world
            .spawn(Transform {
                scale: Vec3::splat(0.001),
                ..Transform::IDENTITY
            })
            .id();
        let child = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)))
            .id();
        propagate_transforms(&mut world);
        let local = local_under_parent(&world, child, Some(parent));
        assert!(
            (local.translation - Vec3::new(1000.0, 2000.0, 3000.0)).length() < 0.1,
            "world (1, 2, 3) under scale 0.001, got {}",
            local.translation
        );
        assert!(
            (local.scale - Vec3::splat(1000.0)).length() < 0.1,
            "local scale {}",
            local.scale
        );
    }

    #[test]
    fn reparent_keep_world_transform_preserves_pose() {
        let mut world = World::new();
        let a = world
            .spawn(Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
            .id();
        let b = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 50.0, 0.0)))
            .id();
        let child = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 1.0, 1.0)))
            .id();
        {
            let mut queue = CommandQueue::default();
            let mut commands = Commands::new(&mut queue, &world);
            commands.entity(child).set_parent(b);
            queue.apply(&mut world);
        }

        // World: child at (1, 51, 1).
        propagate_transforms(&mut world);
        let before = world.get::<GlobalTransform>(child).expect("global").translation();
        assert_eq!(before, Vec3::new(1.0, 51.0, 1.0));

        // Move under `a` keeping world pose.
        {
            let mut queue = CommandQueue::default();
            let mut commands = Commands::new(&mut queue, &world);
            commands.entity(child).set_parent_keep_world_transform(a);
            queue.apply(&mut world);
        }
        propagate_transforms(&mut world);

        let after = world.get::<GlobalTransform>(child).expect("global").translation();
        assert!(
            (after - before).length() < 1e-4,
            "world transform must be preserved: {after:?} vs {before:?}"
        );
        assert_eq!(world.get::<ChildOf>(child).map(|p| p.parent()), Some(a));
        // The local transform now encodes the offset from `a`: world (1, 51, 1) minus a (100, 0, 0).
        let local = world.get::<Transform>(child).expect("local");
        assert!((local.translation - Vec3::new(-99.0, 51.0, 1.0)).length() < 1e-4);
    }

    #[test]
    fn reparent_plain_changes_world_transform() {
        let mut world = World::new();
        let a = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        let child = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        world.entity_mut(child).insert(ChildOf(a));
        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(child).expect("global").translation(),
            Vec3::new(11.0, 0.0, 0.0)
        );
    }

    #[test]
    fn detach_keep_world_writes_global_into_local() {
        let mut world = World::new();
        let parent = world
            .spawn(Transform::from_translation(Vec3::new(5.0, 5.0, 0.0)))
            .id();
        let child = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .insert(ChildOf(parent))
            .id();
        propagate_transforms(&mut world);

        let mut queue = CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, &world);
            commands.entity(child).remove_parent_keep_world_transform();
        }
        queue.apply(&mut world);
        propagate_transforms(&mut world);

        assert!(world.get::<ChildOf>(child).is_none());
        let world_pos = world.get::<GlobalTransform>(child).expect("global").translation();
        assert!((world_pos - Vec3::new(6.0, 5.0, 0.0)).length() < 1e-4);
    }

    /// `despawn_tree` on an inner node takes its subtree only: the parent keeps its other children
    /// (with a consistent `Children` list), and a missing root answers false.
    #[test]
    fn despawn_tree_takes_only_the_subtree() {
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        let kept = world.spawn((Transform::default(), ChildOf(root))).id();
        let mid = world.spawn((Transform::default(), ChildOf(root))).id();
        let leaves: Vec<Entity> = (0..4)
            .map(|_| world.spawn((Transform::default(), ChildOf(mid))).id())
            .collect();
        assert!(despawn_tree(&mut world, mid));
        assert!(world.get_entity(mid).is_err());
        assert!(leaves.iter().all(|e| world.get_entity(*e).is_err()));
        assert!(world.get_entity(kept).is_ok());
        let children: Vec<Entity> = world
            .get::<Children>(root)
            .map(|c| c.to_vec())
            .unwrap_or_default();
        assert_eq!(children, vec![kept]);
        assert!(!despawn_tree(&mut world, mid), "already gone");
    }

    /// A parent with many children: every child goes, nothing else does.
    #[test]
    fn despawn_tree_handles_a_wide_parent() {
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        for _ in 0..20_000 {
            world.spawn((Transform::default(), ChildOf(root)));
        }
        let outsider = world.spawn(Transform::default()).id();
        assert!(despawn_tree(&mut world, root));
        assert!(world.get_entity(outsider).is_ok());
        let mut left = world.query::<&Transform>();
        assert_eq!(left.iter(&world).count(), 1);
    }

    #[test]
    fn despawn_recursive_removes_descendants() {
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        let mid = world.spawn(Transform::default()).insert(ChildOf(root)).id();
        let leaf = world.spawn(Transform::default()).insert(ChildOf(mid)).id();
        let outsider = world.spawn(Transform::default()).id();

        let mut queue = CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, &world);
            commands.entity(root).despawn_recursive();
        }
        queue.apply(&mut world);

        assert!(world.get_entity(root).is_err());
        assert!(world.get_entity(mid).is_err());
        assert!(world.get_entity(leaf).is_err());
        assert!(world.get_entity(outsider).is_ok());
    }
    fn apply(world: &mut World, edit: impl FnOnce(&mut Commands)) {
        let mut queue = CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, world);
            edit(&mut commands);
        }
        queue.apply(world);
    }

    /// Parenting an entity under its own descendant would detach the loop from every root; both
    /// helpers refuse it and leave the hierarchy as it was.
    #[test]
    fn set_parent_refuses_cycles() {
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        let mid = world.spawn(Transform::default()).insert(ChildOf(root)).id();
        let leaf = world.spawn(Transform::default()).insert(ChildOf(mid)).id();

        assert!(would_create_cycle(&world, root, leaf));
        assert!(would_create_cycle(&world, mid, mid));
        assert!(!would_create_cycle(&world, leaf, root));

        apply(&mut world, |commands| {
            commands.entity(root).set_parent(leaf);
        });
        assert!(world.get::<ChildOf>(root).is_none(), "root stays a root");
        apply(&mut world, |commands| {
            commands.entity(mid).set_parent_keep_world_transform(leaf);
        });
        assert_eq!(world.get::<ChildOf>(mid).map(|p| p.parent()), Some(root));

        // A legal move still works: the leaf moves up to the root.
        apply(&mut world, |commands| {
            commands.entity(leaf).set_parent(root);
        });
        assert_eq!(world.get::<ChildOf>(leaf).map(|p| p.parent()), Some(root));
    }

    /// A missing parent is refused instead of leaving a dangling link.
    #[test]
    fn set_parent_to_a_despawned_entity_is_refused() {
        let mut world = World::new();
        let gone = world.spawn_empty().id();
        assert!(world.despawn(gone));
        let child = world.spawn(Transform::default()).id();
        apply(&mut world, |commands| {
            commands.entity(child).set_parent(gone);
        });
        assert!(world.get::<ChildOf>(child).is_none());
    }

    /// The cycle check terminates on an ancestry that is already malformed.
    #[test]
    fn cycle_check_terminates_on_a_malformed_ancestry() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().insert(ChildOf(a)).id();
        world.entity_mut(a).insert(ChildOf(b));
        let outsider = world.spawn_empty().id();
        // Only meaningful if bevy accepted the loop (it refuses self-parenting, not longer cycles).
        if world.get::<ChildOf>(a).is_some() {
            assert!(would_create_cycle(&world, outsider, a));
        }
    }
}
