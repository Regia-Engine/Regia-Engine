// SPDX-License-Identifier: MPL-2.0
//! The play-mode guard for `regia_scene::components::Static` (`specs/14` A1).
//!
//! The marker promises an entity's transform does not change at runtime. The promise is scoped
//! to the **play world**: the edit world moves statics legitimately while building a scene, so
//! the guard is installed on the play app only ([`crate::pie::start_play`]) and never runs on
//! the edit world.
//!
//! Each frame after `TransformPropagate`, `detect_static_moves` visits only the `Static`
//! entities whose [`GlobalTransform`] changed this frame (or that gained the marker) and
//! compares each with its previous observation. Every real pose change counts as one event;
//! the first move of an entity also logs one warning, later moves of the same entity count
//! silently (ADR-5906). The agent surface reads the counter through
//! `editor.static_move_warnings` (the play world cannot be a resource; the panels and agents
//! see the mirror in [`crate::pie::PieStatus`]).

use std::collections::{HashMap, HashSet};

use crate::engine::app::{App, PostUpdate, PostUpdateSet};
use crate::engine::ecs::prelude::*;
use regia_scene::GlobalTransform;
use regia_scene::components::{Name, Static};

/// How many recent events [`StaticMoveWarnings::recent`] keeps.
const RECENT_CAP: usize = 8;

/// One detected runtime move of a `Static` entity in the play world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticMoveEvent {
    /// The entity's display name (its `Name`, else its id), for the warning and the agent.
    pub entity: String,
}

/// Play-mode warnings for `Static` entities that moved at runtime (`specs/14` A1).
///
/// Inserted into the play world by [`install`]; the count is mirrored into the edit world's
/// [`crate::pie::PieStatus`] by the editor loop so panels and the agent surface can read it.
#[derive(Resource, Debug, Default)]
pub struct StaticMoveWarnings {
    /// Move events detected since play started (every pose change of every `Static` entity).
    pub count: u64,
    /// The most recent first moves, one per entity, newest last (at most `RECENT_CAP`, 8).
    /// An entity that keeps moving appears once, so it cannot crowd the others out.
    pub recent: Vec<StaticMoveEvent>,
}

/// The guard's private state.
#[derive(Resource, Default)]
struct StaticGuardState {
    /// Each `Static` entity's last observed pose.
    previous: HashMap<Entity, GlobalTransform>,
    /// Entities already warned about this session: each warns once (ADR-5906).
    warned: HashSet<Entity>,
    /// Entity visits since install (test instrumentation for the change filter).
    #[cfg(test)]
    visited: u64,
}

/// Installs the guard on the play app. Called by [`crate::pie::start_play`] only: the edit
/// world moves statics legitimately, so this never runs there.
pub fn install(app: &mut App) {
    app.init_resource::<StaticMoveWarnings>()
        .init_resource::<StaticGuardState>()
        .add_systems(
            PostUpdate,
            detect_static_moves.after(PostUpdateSet::TransformPropagate),
        );
}

/// The guard's filter: `Static` entities whose world pose changed this frame, or that just
/// gained the marker (their first observation).
type MovedStatics = (With<Static>, Or<(Changed<GlobalTransform>, Added<Static>)>);

/// The guard. A newly spawned entity's first observation never counts (spawning a static is
/// not moving it); each later change of its world transform (its own or a parent's) is one
/// event, and the snapshot then follows the new pose so a held pose stays quiet. Only entities
/// whose `GlobalTransform` changed this frame are visited, so a level of held statics costs
/// nothing per frame. The first event of an entity logs a warning; later ones only count.
fn detect_static_moves(
    mut warnings: ResMut<StaticMoveWarnings>,
    mut state: ResMut<StaticGuardState>,
    mut removed: RemovedComponents<Static>,
    query: Query<(Entity, &GlobalTransform, Option<&Name>), MovedStatics>,
) {
    let _span = tracing::info_span!(target: "regia_editor", "detect_static_moves").entered();
    // Despawned entities and removed markers leave the map (a removal event covers both).
    for entity in removed.read() {
        state.previous.remove(&entity);
        state.warned.remove(&entity);
    }
    for (entity, transform, name) in &query {
        #[cfg(test)]
        {
            state.visited += 1;
        }
        let known = state.previous.insert(entity, *transform);
        if known.as_ref() == Some(transform) || known.is_none() {
            continue; // Held pose, or the entity just spawned (possibly already posed).
        }
        warnings.count += 1;
        if !state.warned.insert(entity) {
            continue; // Warned once already: count only.
        }
        let label = name.map_or_else(|| format!("{entity:?}"), |name| name.as_str().to_owned());
        tracing::warn!(
            target: "regia_editor",
            entity = label,
            "Static entity moved at runtime: remove the Static marker or stop moving it \
             (warned once per entity; editor.static_move_warnings counts every move)"
        );
        if warnings.recent.len() >= RECENT_CAP {
            warnings.recent.remove(0);
        }
        warnings.recent.push(StaticMoveEvent { entity: label });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counts `regia_editor` WARN events on the current thread.
    struct WarnCounter(Arc<AtomicU64>);

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let meta = event.metadata();
            if *meta.level() == tracing::Level::WARN && meta.target() == "regia_editor" {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn pose(x: f32) -> GlobalTransform {
        GlobalTransform(regia_core::glam::Affine3A::from_translation(
            regia_core::glam::Vec3::new(x, 0.0, 0.0),
        ))
    }

    fn visited(world: &World) -> u64 {
        world.resource::<StaticGuardState>().visited
    }

    /// Review finding 5: a held level costs nothing per frame. Only statics whose world pose
    /// changed are visited, not every `Static` entity every frame.
    #[test]
    fn held_statics_are_not_visited_each_frame() {
        let mut world = World::new();
        world.init_resource::<StaticMoveWarnings>();
        world.init_resource::<StaticGuardState>();
        let guard = world.register_system(detect_static_moves);
        let statics: Vec<Entity> = (0..1000)
            .map(|i| world.spawn((Static, pose(i as f32))).id())
            .collect();
        world.run_system(guard).unwrap();
        assert_eq!(visited(&world), 1000, "first observation of every static");
        for _ in 0..3 {
            world.run_system(guard).unwrap();
        }
        assert_eq!(visited(&world), 1000, "held frames visit nothing");
        *world.get_mut::<GlobalTransform>(statics[7]).unwrap() = pose(100.0);
        world.run_system(guard).unwrap();
        assert_eq!(visited(&world), 1001, "one move, one visit");
        assert_eq!(world.resource::<StaticMoveWarnings>().count, 1);
        // A marker added later is observed once (no event), then guarded.
        let late = world.spawn(pose(-1.0)).id();
        world.run_system(guard).unwrap();
        world.entity_mut(late).insert(Static);
        world.run_system(guard).unwrap();
        assert_eq!(
            world.resource::<StaticMoveWarnings>().count,
            1,
            "gaining Static is not a move"
        );
        *world.get_mut::<GlobalTransform>(late).unwrap() = pose(-2.0);
        world.run_system(guard).unwrap();
        assert_eq!(
            world.resource::<StaticMoveWarnings>().count,
            2,
            "the late static is guarded"
        );
        // A despawned static leaves the state.
        world.despawn(statics[3]);
        world.run_system(guard).unwrap();
        assert!(
            !world
                .resource::<StaticGuardState>()
                .previous
                .contains_key(&statics[3])
        );
    }

    /// Review finding 6: an entity moved every frame warns once and keeps counting.
    #[test]
    fn a_static_moved_every_frame_warns_once() {
        let warns = Arc::new(AtomicU64::new(0));
        let dispatch = tracing::Dispatch::new(WarnCounter(Arc::clone(&warns)));
        tracing::dispatcher::with_default(&dispatch, || {
            let mut world = World::new();
            world.init_resource::<StaticMoveWarnings>();
            world.init_resource::<StaticGuardState>();
            let guard = world.register_system(detect_static_moves);
            let statue = world.spawn((Static, pose(0.0), Name::new("Statue"))).id();
            let column = world.spawn((Static, pose(5.0), Name::new("Column"))).id();
            world.run_system(guard).unwrap();
            for frame in 1..=10 {
                *world.get_mut::<GlobalTransform>(statue).unwrap() = pose(frame as f32);
                world.run_system(guard).unwrap();
            }
            *world.get_mut::<GlobalTransform>(column).unwrap() = pose(6.0);
            world.run_system(guard).unwrap();
            let warnings = world.resource::<StaticMoveWarnings>();
            assert_eq!(warnings.count, 11, "every move counts");
            let names: Vec<&str> = warnings.recent.iter().map(|e| e.entity.as_str()).collect();
            assert_eq!(names, ["Statue", "Column"], "one recent entry per mover");
        });
        assert_eq!(
            warns.load(Ordering::Relaxed),
            2,
            "one warning per moved entity, not one per frame"
        );
    }
}
