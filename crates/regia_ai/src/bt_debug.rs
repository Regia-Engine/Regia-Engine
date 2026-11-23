// SPDX-License-Identifier: MPL-2.0
//! A read-only live view of ticking behavior trees for the editor and `ai.bt_live` (ADR-2408).
//!
//! [`BtDebugger`] is a resource that exists only while a debugger is attached: the AI tick looks
//! for it once per pass, and records a [`BtLiveSnapshot`] only for entities someone watches. With
//! no resource (nothing attached) or an empty watch list the tick does no extra work, which the
//! `writes` counter lets a test prove. The snapshot never feeds back into the simulation.

use std::collections::BTreeMap;

use regia_app::prelude::*;

use crate::task::BtStatus;
use crate::tree::BehaviorTree;

/// What a watched tree looked like after its latest tick.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BtLiveSnapshot {
    /// How many ticks have been recorded since the watch began.
    pub ticks: u64,
    /// The root's status from the latest tick.
    pub last_status: BtStatus,
    /// Each node's status from the latest tick that visited it (parallel to the definition).
    pub node_status: Vec<BtStatus>,
    /// For each node, the recorded tick on which its status last changed (0: never changed since
    /// the watch began). A node whose status is `Running` is the editor's running highlight.
    pub changed_at: Vec<u64>,
}

impl BtLiveSnapshot {
    /// Indices of the nodes currently `Running`.
    #[must_use]
    pub fn running_nodes(&self) -> Vec<usize> {
        self.node_status
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == BtStatus::Running)
            .map(|(i, _)| i)
            .collect()
    }
}

/// The attached debugger: the entities being watched and their latest snapshots. Insert it with
/// [`BtDebugger::attach`]; remove the resource to detach everything.
#[derive(Resource, Debug, Default)]
pub struct BtDebugger {
    snapshots: BTreeMap<Entity, BtLiveSnapshot>,
    /// How many snapshots have been written in total (a test counter).
    pub writes: u64,
}

impl BtDebugger {
    /// Starts watching `entity` (inserting the resource when needed).
    pub fn attach(world: &mut World, entity: Entity) {
        world
            .get_resource_or_insert_with(BtDebugger::default)
            .snapshots
            .entry(entity)
            .or_default();
    }

    /// Stops watching `entity`. Returns whether it was watched.
    pub fn detach(&mut self, entity: Entity) -> bool {
        self.snapshots.remove(&entity).is_some()
    }

    /// Whether `entity` is watched.
    #[must_use]
    pub fn watches(&self, entity: Entity) -> bool {
        self.snapshots.contains_key(&entity)
    }

    /// How many entities are watched.
    #[must_use]
    pub fn watched(&self) -> usize {
        self.snapshots.len()
    }

    /// The latest snapshot of a watched entity (default-empty until it ticks once).
    #[must_use]
    pub fn snapshot(&self, entity: Entity) -> Option<&BtLiveSnapshot> {
        self.snapshots.get(&entity)
    }

    /// Records `tree`'s state after a tick, if `entity` is watched.
    pub fn record(&mut self, entity: Entity, tree: &BehaviorTree) {
        let Some(snap) = self.snapshots.get_mut(&entity) else {
            return;
        };
        let now = tree.node_statuses();
        snap.ticks += 1;
        snap.last_status = tree.last_status;
        if snap.node_status.len() != now.len() {
            // First record, or the tree was replaced by one of another size: start over.
            snap.node_status = now.to_vec();
            snap.changed_at = vec![0; now.len()];
        } else {
            for (i, status) in now.iter().enumerate() {
                if snap.node_status[i] != *status {
                    snap.node_status[i] = *status;
                    snap.changed_at[i] = snap.ticks;
                }
            }
        }
        self.writes += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{BtContext, BtTask, BtTaskRegistration};
    use crate::tree::BtTaskRegistry;

    /// Runs for three ticks, then succeeds.
    #[derive(Default)]
    struct ThreeTicks(u32);
    impl BtTask for ThreeTicks {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            self.0 += 1;
            if self.0 > 3 {
                BtStatus::Success
            } else {
                BtStatus::Running
            }
        }
    }
    impl BtTaskRegistration for ThreeTicks {
        fn construct() -> Box<dyn BtTask> {
            Box::new(ThreeTicks::default())
        }
        fn task_type_path() -> &'static str {
            "test::ThreeTicks"
        }
    }

    fn agent(world: &mut World) -> Entity {
        let mut registry = BtTaskRegistry::default();
        registry.register::<ThreeTicks>();
        let mut builder = crate::tree::BehaviorTreeBuilder::new();
        let leaf = builder.leaf("work", "test::ThreeTicks");
        let root = builder.sequence("seq", vec![leaf]);
        let tree = BehaviorTree::compile(&builder.build(root), &registry).expect("compiles");
        world.spawn(tree).id()
    }

    #[test]
    fn nothing_is_written_without_a_debugger() {
        let mut world = World::new();
        let a = agent(&mut world);
        for _ in 0..5 {
            crate::tick_behavior_trees(&mut world);
        }
        assert!(
            !world.contains_resource::<BtDebugger>(),
            "no debugger, no resource"
        );
        // The ticked tree is unaffected: its normal status component is still written.
        assert!(world.get::<crate::tree::BehaviorTreeStatus>(a).is_some());
    }

    #[test]
    fn only_watched_entities_are_recorded() {
        let mut world = World::new();
        let watched = agent(&mut world);
        let other = agent(&mut world);
        // An attached debugger with an empty watch list records nothing.
        world.insert_resource(BtDebugger::default());
        crate::tick_behavior_trees(&mut world);
        assert_eq!(world.resource::<BtDebugger>().writes, 0);
        BtDebugger::attach(&mut world, watched);
        for _ in 0..4 {
            crate::tick_behavior_trees(&mut world);
        }
        let dbg = world.resource::<BtDebugger>();
        assert_eq!(dbg.writes, 4, "one write per tick of the one watched tree");
        assert!(dbg.snapshot(other).is_none());
        assert_eq!(dbg.watched(), 1);
    }

    #[test]
    fn the_snapshot_follows_the_tree_at_fixed_steps() {
        let mut world = World::new();
        let a = agent(&mut world);
        BtDebugger::attach(&mut world, a);
        let mut seen = Vec::new();
        for _ in 0..5 {
            crate::tick_behavior_trees(&mut world);
            let snap = world
                .resource::<BtDebugger>()
                .snapshot(a)
                .cloned()
                .expect("watched");
            let live = world.get::<crate::tree::BehaviorTreeStatus>(a).expect("status");
            assert_eq!(
                snap.node_status, live.node_status,
                "the snapshot is the tree's statuses"
            );
            seen.push((snap.ticks, snap.last_status, snap.running_nodes()));
        }
        assert_eq!(
            seen,
            vec![
                (1, BtStatus::Running, vec![0, 1]),
                (2, BtStatus::Running, vec![0, 1]),
                (3, BtStatus::Running, vec![0, 1]),
                (4, BtStatus::Success, vec![]),
                (5, BtStatus::Success, vec![]),
            ]
        );
        let snap = world
            .resource::<BtDebugger>()
            .snapshot(a)
            .cloned()
            .expect("watched");
        // Both nodes started Running (the first record), and flipped to Success on tick 4.
        assert_eq!(snap.changed_at, vec![4, 4]);
    }

    #[test]
    fn detaching_stops_recording() {
        let mut world = World::new();
        let a = agent(&mut world);
        BtDebugger::attach(&mut world, a);
        crate::tick_behavior_trees(&mut world);
        assert!(world.resource_mut::<BtDebugger>().detach(a));
        crate::tick_behavior_trees(&mut world);
        assert_eq!(world.resource::<BtDebugger>().writes, 1);
    }
}
