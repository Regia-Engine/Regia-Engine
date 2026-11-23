// SPDX-License-Identifier: MPL-2.0
//! Local → global transform and visibility propagation (`docs/specs/14-scene-and-framework.md` A1).
//!
//! Runs as an exclusive system in `PostUpdate/TransformPropagate`. The walk visits only **dirty
//! subtrees**: an entity's [`crate::GlobalTransform`] is recomputed when its own
//! [`crate::Transform`] or [`Visibility`] changed (or it was (re)parented or detached) since the
//! last run, or when its parent was recomputed; ancestors of dirty entities are flagged
//! *subtree-dirty* so the descent from the roots reaches them without recomputing them. A root
//! that is already resolved and whose subtree is clean is not visited, and a clean entity on a
//! dirty path reuses its `GlobalTransform` instead of building another affine. Ancestor marking
//! walks the cached parent up from each changed entity and stops at the first ancestor already
//! flagged, so the marking cost is proportional to the changed paths, not the tree.
//!
//! Change detection is a per-entity snapshot diff (the cache's `tracked` map): each run
//! reads every tracked entity's `Transform`, `Visibility` and `ChildOf` from the cached queries
//! and compares them with the previous run. The tracked set is every entity with a `Transform`, every entity that only
//! links the hierarchy (`ChildOf` without a `Transform` — grouping nodes; reparenting one silently
//! changes its whole subtree's world transforms) and every unparented entity with a `Visibility`
//! (its visibility is inherited by its children). This works identically inside App schedules and
//! when the system is called directly (tests, tools) — bevy's tick-based `Changed` filters only
//! advance their run tick inside a real `System::run`. The comparison is a handful of float ops
//! per entity (≪ the propagation walk itself).
//!
//! A *propagation root* is a tracked entity whose parent is absent or untracked (a bare entity
//! with neither `Transform`, `ChildOf` nor `Visibility`); everything else is reached by descending
//! from its root, exactly once. Descendants without their own `Transform` pass the parent frame
//! through unchanged and still receive a `GlobalTransform` (inserted on first visit), so every
//! entity under a root has a queryable world pose. A `ChildOf` cycle (which bevy does not prevent)
//! has no root, so its entities are never visited: propagation cannot loop on a malformed
//! hierarchy, and the hierarchy helpers in [`crate::hierarchy`] refuse to create one.
//!
//! [`PreviousGlobalTransform`] holds the `GlobalTransform` of the previous run: a recomputed
//! entity gets the value from before the run, and an entity recomputed in the previous run but not
//! in this one catches up to its (unchanged) current value, so a body that stops moving reports
//! zero motion from the next frame on.

use std::collections::HashMap;

use regia_core::math::Affine3A;
use regia_ecs::prelude::*;

use crate::components::{InheritedVisibility, Visibility};
use crate::transform::{GlobalTransform, PreviousGlobalTransform, Transform};

/// Flag: the entity's own transform, visibility or parent link changed this run.
const SELF_DIRTY: u8 = 0b001;
/// Flag: the entity or something below it needs a visit.
const SUBTREE_DIRTY: u8 = 0b010;
/// Flag: the entity's global transform was recomputed during this run's walk.
const RECOMPUTED: u8 = 0b100;

/// What a tracked entity looked like in the previous run. `transform` is `None` for
/// transform-less hierarchy links and visibility-only entities.
#[derive(Clone, Copy, PartialEq)]
struct Tracked {
    transform: Option<Transform>,
    visibility: Option<Visibility>,
    parent: Option<Entity>,
}

/// The parent frame a child inherits during the walk.
#[derive(Clone, Copy)]
struct Frame {
    affine: Affine3A,
    recomputed: bool,
    visible: bool,
}

type TransformState = QueryState<(
    Entity,
    &'static Transform,
    Option<&'static Visibility>,
    Option<&'static ChildOf>,
)>;
type LinkState = QueryState<(Entity, &'static ChildOf, Option<&'static Visibility>), Without<Transform>>;
type VisibilityRootState = QueryState<(Entity, &'static Visibility), (Without<Transform>, Without<ChildOf>)>;

impl Frame {
    const ROOT: Self = Self {
        affine: Affine3A::IDENTITY,
        recomputed: false,
        visible: true,
    };
}

/// Counters of the last propagation run, for perf tests and diagnostics: how much of the world
/// the dirty-subtree walk touched. Deterministic (no timing), so tests can assert on them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropagationStats {
    /// Entities in the tracked set (diffed this run).
    pub tracked: usize,
    /// Entities whose transform, visibility or parent link changed since the previous run.
    pub changed: usize,
    /// Propagation roots the walk started from.
    pub roots: usize,
    /// Entities the walk visited (roots plus every descent into a dirty subtree).
    pub visited: usize,
    /// Entities whose `GlobalTransform` was recomputed.
    pub recomputed: usize,
    /// Entities whose `PreviousGlobalTransform` caught up after they stopped moving.
    pub settled: usize,
    /// `World::get` calls that read the per-entity snapshot in `mark_dirty`. The cached queries
    /// do not increment this; a clean run stays at 0.
    pub snapshot_gets: usize,
    /// `Transform::compute_affine` calls during the walk. A clean root that already has a global
    /// does not build one.
    pub affines: usize,
}

/// Persistent state of the propagation system, kept in a resource so nothing is allocated per
/// frame: the previous-run snapshot for change detection and the walk's scratch buffers.
#[derive(Resource, Default)]
pub struct PropagationCache {
    all_transforms: Option<TransformState>,
    all_links: Option<LinkState>,
    all_visibility_roots: Option<VisibilityRootState>,
    /// Previous-run snapshot per tracked entity: the values, the run that last saw it, and
    /// whether a walk has already produced its global (or its inherited visibility, for a
    /// visibility-only root). The resolved flag is not part of the change comparison.
    tracked: HashMap<Entity, (Tracked, u32, bool)>,
    run: u32,
    flags: HashMap<Entity, u8>,
    recomputed_last_run: Vec<Entity>,
    recomputed_this_run: Vec<Entity>,
    scratch_entities: Vec<Entity>,
    scratch_rows: Vec<(Entity, Tracked)>,
    scratch_dirty: Vec<Entity>,
    scratch_stack: Vec<(Entity, Frame)>,
    scratch_children: Vec<Entity>,
    stats: PropagationStats,
}

impl PropagationCache {
    /// Counters of the most recent [`propagate_transforms`] run.
    pub fn last_run(&self) -> PropagationStats {
        self.stats
    }
}

/// Computes [`GlobalTransform`] and [`InheritedVisibility`] for every dirty subtree.
///
/// Exclusive system, registered by [`crate::ScenePlugin`] in `PostUpdate/TransformPropagate`.
/// Also callable directly (tests, tools): `propagate_transforms(&mut world)`. The counters of the
/// run are readable afterwards through [`PropagationCache::last_run`].
#[tracing::instrument(target = "regia_scene", level = "trace", skip_all)]
pub fn propagate_transforms(world: &mut World) {
    let _span = tracing::debug_span!("propagate_transforms").entered();
    let mut cache = world.remove_resource::<PropagationCache>().unwrap_or_default();
    cache.stats = PropagationStats::default();

    mark_dirty(world, &mut cache);
    propagate_from_roots(world, &mut cache);
    settle_previous(world, &mut cache);

    world.insert_resource(cache);
}

/// Flags edited/reparented entities and their ancestors as dirty.
fn mark_dirty(world: &mut World, cache: &mut PropagationCache) {
    cache.flags.clear();
    cache.scratch_entities.clear();

    if cache.all_transforms.is_none() {
        cache.all_transforms = Some(QueryState::new(world));
    }
    if cache.all_links.is_none() {
        cache.all_links = Some(QueryState::new(world));
    }
    if cache.all_visibility_roots.is_none() {
        cache.all_visibility_roots = Some(QueryState::new(world));
    }

    // The three query filters are disjoint, so every tracked entity is listed once. The component
    // values come from the query; a clean run does not call `World::get`.
    cache.scratch_rows.clear();
    if let Some(state) = cache.all_transforms.as_mut() {
        for (entity, transform, visibility, parent) in state.iter(world) {
            cache.scratch_rows.push((
                entity,
                Tracked {
                    transform: Some(*transform),
                    visibility: visibility.copied(),
                    parent: parent.map(|link| link.parent()),
                },
            ));
        }
    }
    if let Some(state) = cache.all_links.as_mut() {
        for (entity, parent, visibility) in state.iter(world) {
            cache.scratch_rows.push((
                entity,
                Tracked {
                    transform: None,
                    visibility: visibility.copied(),
                    parent: Some(parent.parent()),
                },
            ));
        }
    }
    if let Some(state) = cache.all_visibility_roots.as_mut() {
        for (entity, visibility) in state.iter(world) {
            cache.scratch_rows.push((
                entity,
                Tracked {
                    transform: None,
                    visibility: Some(*visibility),
                    parent: None,
                },
            ));
        }
    }
    cache.stats.tracked = cache.scratch_rows.len();
    cache.scratch_entities.clear();
    cache
        .scratch_entities
        .extend(cache.scratch_rows.iter().map(|(entity, _)| *entity));

    // Diff the tracked entities against the previous run. Fresh spawns have no previous entry
    // and count as changed; detaches show up as a parent change. A changed snapshot is not
    // resolved until the walk writes it.
    cache.run = cache.run.wrapping_add(1);
    let run = cache.run;
    for (entity, current) in &cache.scratch_rows {
        let (changed, resolved) = match cache.tracked.get(entity) {
            Some((tracked, _, was_resolved)) if tracked == current => (false, *was_resolved),
            _ => (true, false),
        };
        cache.tracked.insert(*entity, (*current, run, resolved));
        if changed {
            *cache.flags.entry(*entity).or_insert(0) |= SELF_DIRTY | SUBTREE_DIRTY;
            cache.stats.changed += 1;
        }
    }

    // A `GlobalTransform` removed by hand from a tracked entity (it is required by `Transform`,
    // but can still be removed): the walk inserts it again, so the entity must be visited even
    // when nothing else about it changed (the clean-root skip would leave it without one;
    // corr-ecs-scene finding 6). The removal list is usually empty; an entity that already has
    // its global back is skipped, so a direct caller that never clears the world's trackers does
    // not re-flag it every run.
    for entity in world.removed::<GlobalTransform>() {
        if world.get::<GlobalTransform>(entity).is_some() {
            continue;
        }
        if let Some((_, seen, resolved)) = cache.tracked.get_mut(&entity)
            && *seen == run
        {
            *resolved = false;
            *cache.flags.entry(entity).or_insert(0) |= SELF_DIRTY | SUBTREE_DIRTY;
        }
    }

    // Entities that left the tracked set (despawned, or a grouping node that lost its last
    // `ChildOf`/`Transform`/`Visibility`) are forgotten. A live one no longer passes its parent's
    // frame or visibility down, so its children start walks of their own and must recompute.
    // A live one is a bare root now (no `Transform`, `ChildOf` or `Visibility`), so it is
    // visible: it must not keep the flag a hidden parent gave it (a transform-less light taken out
    // of a hidden group, ADR-7549). Empty, and allocation-free, on a steady run.
    cache.scratch_dirty.clear();
    let left_children = &mut cache.scratch_dirty;
    let mut left: Vec<Entity> = Vec::new();
    cache.tracked.retain(|entity, (_, seen, _)| {
        if *seen == run {
            return true;
        }
        if let Some(children) = world.get::<Children>(*entity) {
            left_children.extend_from_slice(children);
        }
        left.push(*entity);
        false
    });
    for entity in left {
        if let Some(mut inherited) = world.get_mut::<InheritedVisibility>(entity)
            && !inherited.0
        {
            inherited.0 = true;
        }
    }
    while let Some(child) = cache.scratch_dirty.pop() {
        *cache.flags.entry(child).or_insert(0) |= SELF_DIRTY | SUBTREE_DIRTY;
        if let Some((_, _, resolved)) = cache.tracked.get_mut(&child) {
            *resolved = false;
        }
    }

    // Mark ancestors subtree-dirty so the descent from the roots reaches the dirty paths. Stop at
    // the first ancestor already flagged — everything above it is already reachable. The parent
    // is the one just snapshotted. (On a `ChildOf` cycle the walk meets its own flag and stops,
    // so it is bounded too.)
    cache.scratch_dirty.clear();
    cache.scratch_dirty.extend(cache.flags.keys().copied());
    while let Some(mut entity) = cache.scratch_dirty.pop() {
        loop {
            // Copy the parent out before touching `flags`: a `while let` on the map borrow
            // would still be live in the loop body.
            let parent = cache
                .tracked
                .get(&entity)
                .and_then(|(tracked, _, _)| tracked.parent);
            let Some(parent) = parent else { break };
            let flags = cache.flags.entry(parent).or_insert(0);
            if *flags & SUBTREE_DIRTY != 0 {
                break;
            }
            *flags |= SUBTREE_DIRTY;
            entity = parent;
        }
    }
}

/// True when `entity` starts a walk: it has no parent, or its parent is absent from this run's
/// tracked set (a bare entity that carries no frame or visibility to inherit). A tracked parent's
/// descent reaches it instead, so every tracked entity outside a `ChildOf` cycle is visited at
/// most once.
fn is_propagation_root(tracked: &HashMap<Entity, (Tracked, u32, bool)>, run: u32, entity: Entity) -> bool {
    match tracked.get(&entity).and_then(|(snapshot, _, _)| snapshot.parent) {
        None => true,
        Some(parent) => match tracked.get(&parent) {
            Some((_, seen, _)) => *seen != run,
            None => true,
        },
    }
}

/// Walks every dirty subtree from its root, recomputing only what changed.
fn propagate_from_roots(world: &mut World, cache: &mut PropagationCache) {
    cache.scratch_children.clear();
    for entity in &cache.scratch_entities {
        if is_propagation_root(&cache.tracked, cache.run, *entity) {
            cache.scratch_children.push(*entity);
        }
    }
    cache.stats.roots = cache.scratch_children.len();
    cache.scratch_stack.clear();
    for entity in cache.scratch_children.drain(..) {
        let resolved = cache
            .tracked
            .get(&entity)
            .is_some_and(|(_, _, resolved)| *resolved);
        let subtree_dirty = cache
            .flags
            .get(&entity)
            .is_some_and(|flags| flags & SUBTREE_DIRTY != 0);
        // A resolved root with a clean subtree already has the globals its children inherit.
        if resolved && !subtree_dirty {
            continue;
        }
        cache.scratch_stack.push((entity, Frame::ROOT));
    }
    cache.recomputed_this_run.clear();

    while let Some((entity, frame)) = cache.scratch_stack.pop() {
        let Ok(mut entity_mut) = world.get_entity_mut(entity) else {
            continue;
        };
        cache.stats.visited += 1;

        let flag_bits = cache.flags.get(&entity).copied().unwrap_or(0);
        let recompute = flag_bits & SELF_DIRTY != 0 || frame.recomputed;
        let subtree_dirty = flag_bits & SUBTREE_DIRTY != 0;
        let has_transform = entity_mut.get::<Transform>().is_some();
        let in_hierarchy = has_transform || entity_mut.get::<ChildOf>().is_some();
        let missing_global = in_hierarchy && entity_mut.get::<GlobalTransform>().is_none();
        // Build an affine only for a transform that this visit must write. A clean entity that
        // only forwards its frame to a dirty child reuses the global it already has.
        let affine = if has_transform && (recompute || missing_global) {
            cache.stats.affines += 1;
            let local = entity_mut
                .get::<Transform>()
                .map(|transform| transform.compute_affine())
                .unwrap_or(Affine3A::IDENTITY);
            frame.affine * local
        } else if has_transform {
            entity_mut
                .get::<GlobalTransform>()
                .map(|global| global.0)
                .unwrap_or(frame.affine)
        } else {
            frame.affine
        };

        if recompute {
            *cache.flags.entry(entity).or_insert(0) |= RECOMPUTED;
            cache.recomputed_this_run.push(entity);
            cache.stats.recomputed += 1;
            let previous = entity_mut
                .get::<GlobalTransform>()
                .map_or(Affine3A::IDENTITY, |g| g.0);
            if let Some(mut prev) = entity_mut.get_mut::<PreviousGlobalTransform>() {
                prev.0 = previous;
            }
        }
        // Every visited entity in the transform hierarchy gets a queryable world pose, even
        // without its own `Transform` (pass-through grouping nodes, prefab-internal entities). An
        // unparented visibility-only entity is tracked for its visibility alone and gets none.
        if entity_mut.get::<GlobalTransform>().is_none() {
            if in_hierarchy {
                entity_mut.insert(GlobalTransform(affine));
            }
        } else if recompute && let Some(mut global) = entity_mut.get_mut::<GlobalTransform>() {
            global.0 = affine;
        }

        let visible = match entity_mut.get::<Visibility>().copied() {
            Some(Visibility::Visible) => true,
            Some(Visibility::Hidden) => false,
            _ => frame.visible,
        };
        if let Some(mut inherited) = entity_mut.get_mut::<InheritedVisibility>()
            && inherited.0 != visible
        {
            inherited.0 = visible;
        }

        if (recompute || subtree_dirty)
            && let Some(children) = entity_mut.get::<Children>()
        {
            cache.scratch_children.clear();
            cache.scratch_children.extend_from_slice(children);
            let child_frame = Frame {
                affine,
                recomputed: recompute,
                visible,
            };
            for child in cache.scratch_children.drain(..) {
                cache.scratch_stack.push((child, child_frame));
            }
        }
        if let Some((_, _, resolved)) = cache.tracked.get_mut(&entity) {
            *resolved = true;
        }
    }
}

/// Brings `PreviousGlobalTransform` up to date for entities that moved in the previous run but
/// not in this one: their global value did not change this run, so the previous frame's value is
/// the current one. Cost is proportional to what moved, not to the world.
fn settle_previous(world: &mut World, cache: &mut PropagationCache) {
    for entity in &cache.recomputed_last_run {
        if cache.flags.get(entity).is_some_and(|f| f & RECOMPUTED != 0) {
            continue;
        }
        let Ok(mut entity_mut) = world.get_entity_mut(*entity) else {
            continue;
        };
        let Some(current) = entity_mut.get::<GlobalTransform>().map(|g| g.0) else {
            continue;
        };
        if let Some(mut prev) = entity_mut.get_mut::<PreviousGlobalTransform>()
            && prev.0 != current
        {
            prev.0 = current;
            cache.stats.settled += 1;
        }
    }
    std::mem::swap(&mut cache.recomputed_last_run, &mut cache.recomputed_this_run);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ViewVisibility;
    use regia_core::math::{Quat, Vec3};

    fn translation_of(world: &World, entity: Entity) -> Vec3 {
        world
            .get::<GlobalTransform>(entity)
            .expect("global")
            .translation()
    }

    #[test]
    fn composes_hierarchy() {
        let mut world = World::new();
        let root = world
            .spawn(Transform {
                translation: Vec3::new(10.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                ..Transform::IDENTITY
            })
            .with_children(|p| {
                p.spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
                    .with_children(|p| {
                        p.spawn(Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)));
                    });
            })
            .id();
        propagate_transforms(&mut world);

        // +90° about Y maps +X to -Z: child (1,0,0) sits at (10,0,-1).
        let mid = world.get::<Children>(root).expect("children")[0];
        assert!((translation_of(&world, mid) - Vec3::new(10.0, 0.0, -1.0)).length() < 1e-5);
        let leaf = world.get::<Children>(mid).expect("children")[0];
        assert!((translation_of(&world, leaf) - Vec3::new(10.0, 2.0, -1.0)).length() < 1e-5);
    }

    #[test]
    fn uniform_scale_composes() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_scale(2.0))
            .with_children(|p| {
                p.spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)));
            })
            .id();
        propagate_transforms(&mut world);
        let child = world.get::<Children>(root).expect("children")[0];
        assert!((translation_of(&world, child) - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-5);
    }

    #[test]
    fn dirty_subtree_only_updates_what_changed() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .with_children(|p| {
                p.spawn(Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)));
            })
            .id();
        let other = world
            .spawn(Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
            .with_children(|p| {
                p.spawn(Transform::from_translation(Vec3::new(0.0, 100.0, 0.0)));
            })
            .id();
        propagate_transforms(&mut world);
        // A settle run: everything was recomputed by the first run, so this one only brings the
        // previous-frame values up to the (unchanged) spawn pose (ADR-876).
        propagate_transforms(&mut world);

        let child = world.get::<Children>(root).expect("children")[0];
        let other_child = world.get::<Children>(other).expect("children")[0];
        assert_eq!(translation_of(&world, child), Vec3::new(1.0, 1.0, 0.0));

        // Move `child` only: the clean sibling subtree must be untouched.
        let root_global_before = world.get::<GlobalTransform>(root).expect("g").0;
        let other_global_before = world.get::<GlobalTransform>(other).expect("g").0;
        let other_child_global_before = world.get::<GlobalTransform>(other_child).expect("g").0;
        let other_child_prev_before = world.get::<PreviousGlobalTransform>(other_child).expect("p").0;
        world.get_mut::<Transform>(child).expect("t").translation = Vec3::new(0.0, 1.0, 5.0);

        propagate_transforms(&mut world);

        assert_eq!(translation_of(&world, child), Vec3::new(1.0, 1.0, 5.0));
        assert_eq!(
            world.get::<GlobalTransform>(root).expect("g").0,
            root_global_before
        );
        assert_eq!(
            world.get::<GlobalTransform>(other).expect("g").0,
            other_global_before
        );
        assert_eq!(
            world.get::<GlobalTransform>(other_child).expect("g").0,
            other_child_global_before
        );
        // The clean entity's previous-frame value is equally untouched.
        assert_eq!(
            world.get::<PreviousGlobalTransform>(other_child).expect("p").0,
            other_child_prev_before
        );
        // ...while the moved entity's previous frame holds the value from before this run.
        assert_eq!(
            Vec3::from(
                world
                    .get::<PreviousGlobalTransform>(child)
                    .expect("p")
                    .0
                    .translation
            ),
            Vec3::new(1.0, 1.0, 0.0)
        );
    }

    #[test]
    fn previous_global_tracks_last_frame() {
        let mut world = World::new();
        let entity = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        propagate_transforms(&mut world);
        world.get_mut::<Transform>(entity).expect("t").translation = Vec3::new(2.0, 0.0, 0.0);
        propagate_transforms(&mut world);
        let prev = world.get::<PreviousGlobalTransform>(entity).expect("prev");
        assert_eq!(Vec3::from(prev.0.translation), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(translation_of(&world, entity), Vec3::new(2.0, 0.0, 0.0));
    }

    #[test]
    fn transformless_intermediate_passes_frame_through() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)))
            .with_children(|p| {
                p.spawn_empty().with_children(|p| {
                    p.spawn(Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)));
                });
            })
            .id();
        propagate_transforms(&mut world);
        let mid = world.get::<Children>(root).expect("children")[0];
        let leaf = world.get::<Children>(mid).expect("children")[0];
        assert!((translation_of(&world, leaf) - Vec3::new(1.0, 2.0, 3.0)).length() < 1e-6);
    }

    #[test]
    fn visibility_inherits_and_overrides() {
        let mut world = World::new();
        let root = world
            .spawn((Transform::default(), Visibility::Hidden))
            .with_children(|p| {
                p.spawn((Transform::default(), Visibility::Inherited))
                    .with_children(|p| {
                        p.spawn((Transform::default(), Visibility::Visible));
                    });
            })
            .id();
        propagate_transforms(&mut world);
        let child = world.get::<Children>(root).expect("children")[0];
        let grandchild = world.get::<Children>(child).expect("children")[0];
        assert!(!world.get::<InheritedVisibility>(root).expect("v").0);
        assert!(!world.get::<InheritedVisibility>(child).expect("v").0);
        assert!(world.get::<InheritedVisibility>(grandchild).expect("v").0);
    }

    /// An entity carrying only `InheritedVisibility` (a required component of a light or a mesh)
    /// under a hidden parent resolves hidden; detached, it leaves the tracked set (no
    /// `Transform`, `ChildOf` or `Visibility`) and is a bare root again, so it must not keep the
    /// hidden parent's flag (w6-post05b-fixes, ADR-7549).
    #[test]
    fn an_entity_leaving_the_tracked_set_drops_its_hidden_parents_visibility() {
        let mut world = World::new();
        let parent = world.spawn((Transform::default(), Visibility::Hidden)).id();
        let child = world
            .spawn((InheritedVisibility::default(), ChildOf(parent)))
            .id();
        propagate_transforms(&mut world);
        assert!(
            !world.get::<InheritedVisibility>(child).expect("v").0,
            "hidden with its parent"
        );
        world.entity_mut(child).remove::<ChildOf>();
        propagate_transforms(&mut world);
        assert!(
            world.get::<InheritedVisibility>(child).expect("v").0,
            "a bare root is visible"
        );
        // A despawned entity is simply forgotten.
        let gone = world
            .spawn((InheritedVisibility::default(), ChildOf(parent)))
            .id();
        propagate_transforms(&mut world);
        world.despawn(gone);
        propagate_transforms(&mut world);
    }

    #[test]
    fn detached_and_reparented_entities_recompute() {
        let mut world = World::new();
        let a = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        let child = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        world.entity_mut(child).insert(ChildOf(a));
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, child), Vec3::new(11.0, 0.0, 0.0));

        // Detach: now a root at its local (1,0,0).
        world.entity_mut(child).remove::<ChildOf>();
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, child), Vec3::new(1.0, 0.0, 0.0));
    }

    #[test]
    fn required_components_are_inserted() {
        let mut world = World::new();
        let e = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        assert!(world.get::<GlobalTransform>(e).is_some());
        assert!(world.get::<PreviousGlobalTransform>(e).is_some());
        let v = world.spawn(Visibility::default()).id();
        assert!(world.get::<InheritedVisibility>(v).is_some());
        assert!(world.get::<ViewVisibility>(v).is_some());
    }

    /// A transform-less parent must not be visited as a false root: the child's world pose comes
    /// from the real root through the pass-through chain, regardless of entity spawn order.
    #[test]
    fn transformless_parent_is_not_a_root() {
        let mut world = World::new();
        // Spawn the child FIRST so entity order would visit it before its root (the old
        // single-level root check double-visited it and overwrote the pose with identity).
        let leaf = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)))
            .id();
        let mid = world.spawn_empty().id();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        world.entity_mut(leaf).insert(ChildOf(mid));
        world.entity_mut(mid).insert(ChildOf(root));
        propagate_transforms(&mut world);
        assert!(
            (translation_of(&world, leaf) - Vec3::new(10.0, 1.0, 0.0)).length() < 1e-5,
            "leaf must inherit through the transform-less intermediate"
        );
        // Stable across further runs (nothing dirty).
        propagate_transforms(&mut world);
        assert!(
            (translation_of(&world, leaf) - Vec3::new(10.0, 1.0, 0.0)).length() < 1e-5,
            "clean run must not disturb the pose"
        );
    }

    /// Reparenting a transform-less intermediate changes its descendants' world poses; the
    /// parent-link diff must catch it even though no `Transform` changed.
    #[test]
    fn reparenting_a_transformless_intermediate_propagates() {
        let mut world = World::new();
        let a = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        let b = world
            .spawn(Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)))
            .id();
        let mid = world.spawn_empty().id();
        let leaf = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)))
            .id();
        world.entity_mut(mid).insert(ChildOf(a));
        world.entity_mut(leaf).insert(ChildOf(mid));
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, leaf), Vec3::new(10.0, 2.0, 0.0));

        world.entity_mut(mid).insert(ChildOf(b));
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, leaf), Vec3::new(50.0, 2.0, 0.0));
    }

    /// Entities under a transform root get a `GlobalTransform` even without their own
    /// `Transform` (pass-through grouping nodes, prefab-internal entities).
    #[test]
    fn transformless_entities_receive_a_global_transform() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(4.0, 0.0, 0.0)))
            .with_children(|p| {
                p.spawn_empty();
            })
            .id();
        let mid = world.get::<Children>(root).expect("children")[0];
        propagate_transforms(&mut world);
        assert_eq!(
            world.get::<GlobalTransform>(mid).expect("global").translation(),
            Vec3::new(4.0, 0.0, 0.0)
        );
    }

    /// A `Visibility` edit with no transform change still reaches the children (the diff tracks
    /// visibility; before ADR-876 only the root's own `InheritedVisibility` updated).
    #[test]
    fn visibility_edit_alone_reaches_descendants() {
        let mut world = World::new();
        let root = world
            .spawn((Transform::default(), Visibility::Inherited))
            .with_children(|p| {
                p.spawn((Transform::default(), Visibility::Inherited))
                    .with_children(|p| {
                        p.spawn((Transform::default(), Visibility::Inherited));
                    });
            })
            .id();
        let child = world.get::<Children>(root).expect("children")[0];
        let grandchild = world.get::<Children>(child).expect("children")[0];
        propagate_transforms(&mut world);
        assert!(world.get::<InheritedVisibility>(grandchild).expect("v").0);

        *world.get_mut::<Visibility>(root).expect("vis") = Visibility::Hidden;
        propagate_transforms(&mut world);
        assert!(!world.get::<InheritedVisibility>(root).expect("v").0);
        assert!(!world.get::<InheritedVisibility>(child).expect("v").0);
        assert!(!world.get::<InheritedVisibility>(grandchild).expect("v").0);

        *world.get_mut::<Visibility>(root).expect("vis") = Visibility::Inherited;
        propagate_transforms(&mut world);
        assert!(world.get::<InheritedVisibility>(grandchild).expect("v").0);

        // Hiding the middle entity hides only its subtree.
        *world.get_mut::<Visibility>(child).expect("vis") = Visibility::Hidden;
        propagate_transforms(&mut world);
        assert!(world.get::<InheritedVisibility>(root).expect("v").0);
        assert!(!world.get::<InheritedVisibility>(grandchild).expect("v").0);
    }

    /// Visibility-only entities resolve too: a hidden one without a `Transform` reads hidden, and
    /// a transform-less hidden parent hides its transform children. Neither gets a
    /// `GlobalTransform` it did not have.
    #[test]
    fn visibility_only_entities_resolve_and_hide_their_children() {
        let mut world = World::new();
        let lone = world.spawn(Visibility::Hidden).id();
        let group = world
            .spawn(Visibility::Hidden)
            .with_children(|p| {
                p.spawn((
                    Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
                    Visibility::Inherited,
                ));
            })
            .id();
        let member = world.get::<Children>(group).expect("children")[0];
        propagate_transforms(&mut world);

        assert!(!world.get::<InheritedVisibility>(lone).expect("v").0);
        assert!(!world.get::<InheritedVisibility>(member).expect("v").0);
        assert!(world.get::<GlobalTransform>(lone).is_none());
        assert!(world.get::<GlobalTransform>(group).is_none());
        assert_eq!(translation_of(&world, member), Vec3::new(2.0, 0.0, 0.0));

        *world.get_mut::<Visibility>(group).expect("vis") = Visibility::Visible;
        propagate_transforms(&mut world);
        assert!(world.get::<InheritedVisibility>(member).expect("v").0);
    }

    /// After an entity stops moving, its previous-frame transform catches up on the next run, so
    /// motion vectors read zero instead of repeating the last motion forever.
    #[test]
    fn previous_global_settles_when_an_entity_stops() {
        let mut world = World::new();
        let entity = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        let prev_of = |world: &World| {
            Vec3::from(
                world
                    .get::<PreviousGlobalTransform>(entity)
                    .expect("prev")
                    .0
                    .translation,
            )
        };
        propagate_transforms(&mut world);
        // First frame after spawn: identity, by definition.
        assert_eq!(prev_of(&world), Vec3::ZERO);
        propagate_transforms(&mut world);
        assert_eq!(prev_of(&world), Vec3::new(1.0, 0.0, 0.0));

        world.get_mut::<Transform>(entity).expect("t").translation = Vec3::new(3.0, 0.0, 0.0);
        propagate_transforms(&mut world);
        assert_eq!(prev_of(&world), Vec3::new(1.0, 0.0, 0.0), "moving frame");
        assert_eq!(translation_of(&world, entity), Vec3::new(3.0, 0.0, 0.0));

        // Stopped: the next run reports no motion, and the one after touches nothing.
        propagate_transforms(&mut world);
        assert_eq!(prev_of(&world), Vec3::new(3.0, 0.0, 0.0), "stopped frame");
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!((stats.recomputed, stats.settled), (0, 1), "{stats:?}");
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!((stats.recomputed, stats.settled), (0, 0), "{stats:?}");
    }

    /// Children of a moving parent settle as well (they were recomputed through the parent).
    #[test]
    fn previous_global_settles_for_descendants() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::default())
            .with_children(|p| {
                p.spawn(Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)));
            })
            .id();
        let child = world.get::<Children>(root).expect("children")[0];
        propagate_transforms(&mut world);
        propagate_transforms(&mut world);
        world.get_mut::<Transform>(root).expect("t").translation = Vec3::new(5.0, 0.0, 0.0);
        propagate_transforms(&mut world);
        propagate_transforms(&mut world);
        let prev = world.get::<PreviousGlobalTransform>(child).expect("prev").0;
        let global = world.get::<GlobalTransform>(child).expect("global").0;
        assert_eq!(prev, global);
        assert_eq!(Vec3::from(global.translation), Vec3::new(5.0, 1.0, 0.0));
    }

    /// A `ChildOf` cycle (bevy does not refuse one) must not hang the walk; the rest of the world
    /// still propagates.
    #[test]
    fn a_hierarchy_cycle_does_not_hang_propagation() {
        let mut world = World::new();
        let a = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        let b = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)))
            .id();
        world.entity_mut(b).insert(ChildOf(a));
        world.entity_mut(a).insert(ChildOf(b));
        // A transform-less cycle with a transform child: the old root search walked up through
        // transform-less parents and never returned.
        let c = world.spawn_empty().id();
        let d = world.spawn_empty().insert(ChildOf(c)).id();
        world.entity_mut(c).insert(ChildOf(d));
        world
            .spawn(Transform::from_translation(Vec3::new(0.0, 0.0, 3.0)))
            .insert(ChildOf(d));
        let healthy = world
            .spawn(Transform::from_translation(Vec3::new(7.0, 0.0, 0.0)))
            .id();
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, healthy), Vec3::new(7.0, 0.0, 0.0));
        // A second run (with the cycle members now in the tracked map) terminates as well.
        world.get_mut::<Transform>(a).expect("t").translation = Vec3::new(2.0, 0.0, 0.0);
        propagate_transforms(&mut world);
        world.get_mut::<Transform>(healthy).expect("t").translation = Vec3::new(8.0, 0.0, 0.0);
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, healthy), Vec3::new(8.0, 0.0, 0.0));
    }

    /// The dirty-subtree walk's cost, measured in visits (deterministic): a clean world visits
    /// nothing, and moving one leaf of a 1 + 100 + 1000 tree visits the root, its 100
    /// children and the 10 grandchildren under the moved leaf's parent, recomputing exactly one.
    #[test]
    fn walk_cost_follows_the_dirty_path() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::default())
            .with_children(|p| {
                for i in 0..100 {
                    p.spawn(Transform::from_translation(Vec3::new(i as f32, 0.0, 0.0)))
                        .with_children(|p| {
                            for j in 0..10 {
                                p.spawn(Transform::from_translation(Vec3::new(0.0, j as f32, 0.0)));
                            }
                        });
                }
            })
            .id();
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!(stats.tracked, 1101);
        assert_eq!((stats.changed, stats.recomputed), (1101, 1101), "{stats:?}");

        propagate_transforms(&mut world); // settle
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!(
            (stats.roots, stats.visited, stats.recomputed, stats.settled),
            (1, 0, 0, 0),
            "clean run: {stats:?}"
        );

        let mid = world.get::<Children>(root).expect("children")[42];
        let leaf = world.get::<Children>(mid).expect("children")[7];
        world.get_mut::<Transform>(leaf).expect("t").translation.z = 9.0;
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!(
            (stats.changed, stats.visited, stats.recomputed),
            (1, 1 + 100 + 10, 1),
            "{stats:?}"
        );
        assert_eq!(
            translation_of(&world, leaf),
            Vec3::new(42.0, 7.0, 9.0),
            "the moved leaf composes with its parent"
        );
    }

    /// Detaching a transform-less grouping node takes it out of the hierarchy: its children no
    /// longer inherit the old parent's frame (they used to keep the stale pose, since nothing they
    /// carry changed). Removing a hiding group's `Visibility` likewise un-hides its children.
    #[test]
    fn entities_leaving_the_hierarchy_recompute_their_children() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        let group = world.spawn_empty().insert(ChildOf(root)).id();
        let leaf = world
            .spawn(Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)))
            .insert(ChildOf(group))
            .id();
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, leaf), Vec3::new(10.0, 1.0, 0.0));

        world.entity_mut(group).remove::<ChildOf>();
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, leaf), Vec3::new(0.0, 1.0, 0.0));

        let hider = world
            .spawn(Visibility::Hidden)
            .with_children(|p| {
                p.spawn((Transform::default(), Visibility::Inherited));
            })
            .id();
        let hidden = world.get::<Children>(hider).expect("children")[0];
        propagate_transforms(&mut world);
        assert!(!world.get::<InheritedVisibility>(hidden).expect("v").0);
        world.entity_mut(hider).remove::<Visibility>();
        propagate_transforms(&mut world);
        assert!(world.get::<InheritedVisibility>(hidden).expect("v").0);
    }

    /// `ChildOf` does not reject cycles. A cycle has no entry from outside (each entity has one
    /// parent), so the descent never reaches it: propagation terminates, and a normal tree
    /// beside the cycle still resolves.
    #[test]
    fn a_child_of_cycle_terminates_and_leaves_other_trees_alone() {
        let mut world = World::new();
        let a = world.spawn(Transform::from_translation(Vec3::X)).id();
        let b = world
            .spawn((Transform::from_translation(Vec3::Y), ChildOf(a)))
            .id();
        world.spawn((Transform::default(), ChildOf(b)));
        world.entity_mut(a).insert(ChildOf(b));
        let bare_a = world.spawn_empty().id();
        let bare_b = world.spawn(ChildOf(bare_a)).id();
        world.entity_mut(bare_a).insert(ChildOf(bare_b));
        let root = world
            .spawn(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
            .id();
        let leaf = world
            .spawn((Transform::from_translation(Vec3::Z), ChildOf(root)))
            .id();
        for _ in 0..3 {
            propagate_transforms(&mut world);
        }
        assert_eq!(translation_of(&world, leaf), Vec3::new(5.0, 0.0, 1.0));
    }

    #[test]
    fn visibility_edit_after_first_run_reaches_descendants() {
        let mut world = World::new();
        let root = world
            .spawn((Transform::default(), Visibility::Visible))
            .with_children(|p| {
                // `Visibility` brings the `InheritedVisibility` the edit must reach.
                p.spawn((Transform::default(), Visibility::Inherited))
                    .with_children(|p| {
                        p.spawn((Transform::default(), Visibility::Inherited));
                    });
            })
            .id();
        propagate_transforms(&mut world);
        let child = world.get::<Children>(root).expect("children")[0];
        let grandchild = world.get::<Children>(child).expect("children")[0];
        assert!(world.get::<InheritedVisibility>(grandchild).expect("v").0);

        world.entity_mut(root).insert(Visibility::Hidden);
        propagate_transforms(&mut world);
        assert!(!world.get::<InheritedVisibility>(child).expect("v").0);
        assert!(!world.get::<InheritedVisibility>(grandchild).expect("v").0);
    }

    /// A flat level of parentless props with nothing moved must not diff by random `World::get`,
    /// visit the roots, or build an affine. The first run still fills every global.
    #[test]
    fn clean_flat_world_skips_unchanged_roots() {
        let mut world = World::new();
        let mut ids = Vec::with_capacity(1000);
        for i in 0..1000 {
            ids.push(
                world
                    .spawn(Transform::from_translation(Vec3::new(i as f32, 1.0, 0.0)))
                    .id(),
            );
        }
        propagate_transforms(&mut world);
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!(stats.tracked, 1000, "{stats:?}");
        assert_eq!(
            (
                stats.changed,
                stats.visited,
                stats.recomputed,
                stats.snapshot_gets,
                stats.affines
            ),
            (0, 0, 0, 0, 0),
            "clean flat run: {stats:?}"
        );
        assert_eq!(translation_of(&world, ids[42]), Vec3::new(42.0, 1.0, 0.0));

        world.get_mut::<Transform>(ids[42]).expect("t").translation.x = 7.0;
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!(
            (stats.changed, stats.visited, stats.recomputed),
            (1, 1, 1),
            "one moved prop: {stats:?}"
        );
        assert_eq!(translation_of(&world, ids[42]), Vec3::new(7.0, 1.0, 0.0));
        assert_eq!(translation_of(&world, ids[43]), Vec3::new(43.0, 1.0, 0.0));
    }

    /// corr-low-framework (corr-ecs-scene finding 6): a `GlobalTransform` removed by hand from a
    /// clean, resolved entity comes back on the next run. The clean-root skip (perf change) never
    /// visited the root again, so it stayed without one until its `Transform` changed.
    #[test]
    fn a_removed_global_transform_comes_back_on_a_clean_entity() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)))
            .id();
        let child = world
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
                ChildOf(root),
            ))
            .id();
        propagate_transforms(&mut world);
        propagate_transforms(&mut world);
        world.entity_mut(root).remove::<GlobalTransform>();
        world.entity_mut(child).remove::<GlobalTransform>();
        propagate_transforms(&mut world);
        assert_eq!(translation_of(&world, root), Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(translation_of(&world, child), Vec3::new(1.0, 2.0, 0.0));
        // Restored once: later runs are clean again.
        propagate_transforms(&mut world);
        let stats = world.resource::<PropagationCache>().last_run();
        assert_eq!((stats.visited, stats.recomputed), (0, 0), "{stats:?}");
    }

    #[test]
    fn transformless_parent_cycle_terminates() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn(ChildOf(a)).id();
        world.entity_mut(a).insert(ChildOf(b));
        propagate_transforms(&mut world);
        assert!(world.entities().contains(a));
    }
}
