// SPDX-License-Identifier: MPL-2.0
//! Game AI (`docs/specs/22-tier2.md` § T2.2): behaviour trees driving AI pawns, a reflected
//! blackboard, and perception helpers.
//!
//! # How the pieces fit
//!
//! An AI agent entity carries a [`BehaviorTree`] (compiled from a [`BehaviorTreeDef`] — the
//! reflected `.bt.ron` form), a [`Blackboard`] (reflected key/value store shared with the tree's
//! nodes and leaf tasks) and optionally a [`Perception`] component. [`AiPlugin`] runs one exclusive
//! system each `FixedUpdate` that ticks every tree with take-run-reinsert semantics, so leaf tasks
//! get exclusive world access through the [`BtContext`]. Leaf tasks are Rust types implementing
//! [`BtTask`] and registered with the `#[derive(BtTask)]` derive ([`BtTaskRegistration`]); they are
//! looked up in a tree definition by their type path.
//!
//! # Example
//!
//! ```ignore
//! #[derive(BtTask, Default)]
//! struct SeekTarget;
//! impl BtTask for SeekTarget {
//!     fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus { /* ... */ BtStatus::Running }
//! }
//!
//! // At registration time:
//! app.world_mut().resource_mut::<BtTaskRegistry>().register::<SeekTarget>();
//! ```
//!
//! See `book/user-guide/navigation-ai.md` and `book/agents/navigation.md`.

pub mod abilities;
pub mod attack_tokens;
pub mod blackboard;
pub mod bt_debug;
pub mod bt_edit;
pub mod formation;
pub mod formation_asset;
pub mod goap;
pub mod goap_ecs;
pub mod logic;
// Logic graph editing: the node document, ops and validator the editor and agent share (spec 102 P2).
pub mod logic_edit;
#[cfg(feature = "net")]
pub mod net_bots;
pub mod perception;
pub mod perception_hysteresis;
pub mod perception_system;
pub mod smart_object;
pub mod smart_object_def;
pub mod smart_object_load;
pub mod smart_object_v2;
pub mod spatial_query;
pub mod squad;
pub mod state_tree;
pub mod state_tree_edit;
pub mod task;
pub mod tick_budget;
pub mod tree;
pub mod utility;
pub mod utility_ecs;

pub mod bt_bridge;
pub mod bt_value;
// The `PathProvider` seam and the `MoveTo` leaf over the three navigation stacks (ADR-8293).
pub mod debug_gizmos;
pub mod path_provider;

pub use abilities::{Abilities, Ability, AbilityDef, AbilityError, tick_abilities};
pub use attack_tokens::{
    AcquireAttackToken, AiAttackTokens, AiTokenHolder, HasAttackToken, ReleaseAttackToken, TokenHold,
    TokenOutcome, TokenWaiter, acquire_token, holds_token, release_all_tokens, release_token,
    set_token_capacity,
};
pub use blackboard::{BbValue, Blackboard};
pub use bt_debug::{BtDebugger, BtLiveSnapshot};
#[cfg(feature = "debug_draw")]
pub use formation::draw_formation_gizmos;
pub use formation::{AiFormation, FormationAnchor, FormationShape, FormationSlot, resolve_formation};
pub use formation_asset::{
    AssignMember, AssignSlot, FORMATION_FORMAT_VERSION, FORMATION_HUNGARIAN_MAX, FormationAsset,
    FormationAssetError, FormationFile, FormationSlotDesc, FormationSlotFile, SlotAssignment, SlotOffset,
    assign_slots, formation_migration_sources, formation_schema_fingerprint, horizontal_distance_um,
    migrate_formation_ron,
};
pub use goap_ecs::{GoapAgent, GoapPlugin, GoapSettings, GoapState, GoapStats, GoapStatus, GoapStep};
pub use logic::{LogicAuthority, LogicGraph, LogicGraphDef, LogicGraphStatus, LogicInbox};
pub use perception::{HearingEvent, Perception, can_hear, in_sight_cone, line_of_sight};
#[cfg(feature = "debug_draw")]
pub use perception::{draw_memory_point, draw_meter_bar, draw_perception, draw_senses, draw_team_marker};
pub use perception_hysteresis::{
    HysteresisReject, SightRanges, assign_hysteresis, effective_lose_half_angle, effective_lose_radius,
    hysteresis_accepted, sight_holds,
};
pub use perception_system::{
    Attitude, AttitudeFilter, Attitudes, DamageEvent, DetectionSample, LostReason, Perceivable, Perceived,
    PerceptionGained, PerceptionLost, PerceptionPlugin, PerceptionSenses, PerceptionSettings,
    PerceptionStats, PerceptionStep, Sense, SenseListed, SenseRegisterError, SharedStimulus, SightSamples,
    Stimulus, TeamKnowledge, Visibility,
};
pub use regia_macros::BtTask;
pub use smart_object::{
    SlotInfo, SmartObject, SmartObjectError, SmartObjectIndex, SmartObjectPlugin, SmartObjectSlot,
    claim_slot, effective_enabled, release_slot, smart_object_ancestor,
};
pub use smart_object_def::{SmartObjectDef, SmartObjectDefError, SmartObjectDefinition, materialize_object};
pub use smart_object_v2::{
    Availability, ClaimHandle, ClaimPriority, QueryRegion, RejectionLog, SlotChanged, SlotHit, SlotLost,
    SlotQuery, SlotState, SmartObjectEvents, SmartObjectRegistry, begin_use, claim_with, release_handle,
};
pub use spatial_query::{
    RunSpatialQuery, SpatialQueryAsset, SpatialQueryManager, SpatialQueryPlugin, SpatialQueryProbe,
    SpatialQueryRequest, SpatialQueryResult,
};
pub use squad::{
    AiDead, AiSquad, AiSquadClock, AiSquadMember, AiSquadPlugin, AiSquadStep, SQUAD_IS_LEADER_KEY,
    SQUAD_LEADER_KEY, SQUAD_ROLE_KEY, SquadError, SquadRead, SquadRole, SquadWrite, claim_squad_role,
    release_squad_role, set_squad_leader, set_squad_role, squad_of,
};
#[cfg(debug_assertions)]
pub use state_tree::StateTreeDebug;
pub use state_tree::{StateTree, StateTreeDef, StateTreeInbox, StateTreeRunStatus, StateTreeStatus};
pub use task::{BtContext, BtStatus, BtTask, BtTaskRegistration};
pub use tick_budget::{AiImportance, AiTickAgentInfo, AiTickBudget, AiTickStats, AiTickTier};
pub use tree::{
    BehaviorTree, BehaviorTreeBuilder, BehaviorTreeDef, BehaviorTreeStatus, BtCompileError, BtTaskRegistry,
    DefKind, DefNode, MAX_BT_DEPTH, ParallelPolicy,
};
pub use utility::{
    ActionDef, ActionScore, BlockReason, ConsiderationDef, ConsiderationScore, CurveKind, CurvePoint,
    DecisionReport, Reasoner, ResponseCurve, SelectionMode, UtilityAiDef, UtilityAiError, compensate,
    modification_factor,
};
pub use utility_ecs::{UtilityClock, UtilityDefs, UtilityPlugin, UtilityReasoner, UtilityStep};

pub use bt_edit::migrate_bt_ron;
pub use bt_value::{AbortMode, BtCompareOp, BtLiteral, BtValueKind};
pub use debug_gizmos::{AiDebugDraw, AiDebugFilter, AiDebugLayers, active_leaf_name};
pub use path_provider::{
    CrowdSteered, DetGridProvider, DetNavMeshProvider, MOVE_TO_TASK, MoveTarget, MoveTo, MoveToStatus,
    NavMeshProvider, PathAgentSpec, PathProvider, PathProviders, PathRequestOutcome,
};
/// The deterministic AI core for lockstep games (plan row S-09, ADR-8152): `regia_detsim::ai`, re-exported.
///
/// ECS-free, so a lockstep sim links `regia_detsim` alone; ECS games reach it here. Its scorer and curves have
/// Direct Order 27's exact bits, which differ from this crate's [`utility`] reasoner (ADR-4773 kernels).
pub use regia_detsim::ai as det;

use regia_app::prelude::*;
use regia_scene::layers::LayerParked;

/// Installs the AI systems: registers the reflected types, the [`HearingEvent`] message and the
/// [`BtTaskRegistry`], and ticks every [`Abilities`] cooldown and then every behaviour tree in
/// `FixedUpdate` (in [`AiStep`]).
#[derive(Default)]
pub struct AiPlugin;

impl Plugin for AiPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(debug_assertions)]
        app.register_type::<StateTreeDebug>();
        #[cfg(debug_assertions)]
        app.register_type::<logic::LogicGraphDebug>();
        // Tick order is the order the trees arrived (ADR-2396). Tracking only starts while no
        // entity holds the component yet, which is the case when the plugin builds; a re-insert
        // by the take-run-reinsert tick renumbers agents in the order they ticked, so the order
        // stays the same from tick to tick.
        regia_ecs::spawn_order::track_spawn_order::<BehaviorTree>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<StateTree>(app.world_mut());
        // The spatial-query seed prefers the blackboard's arrival over the behavior tree's
        // (`owner_seed_key`): the tree tick takes `BehaviorTree` out of the agent while a leaf
        // runs, so that table loses the owner mid-leaf and an instant query would otherwise
        // seed from entity bits. The blackboard is never taken.
        regia_ecs::spawn_order::track_spawn_order::<Blackboard>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<LogicGraph>(app.world_mut());
        app.register_type::<Blackboard>()
            .register_type::<BbValue>()
            .register_type::<BehaviorTreeDef>()
            .register_type::<DefNode>()
            .register_type::<DefKind>()
            .register_type::<ParallelPolicy>()
            .register_type::<AbortMode>()
            .register_type::<BtCompareOp>()
            .register_type::<BtValueKind>()
            .register_type::<BtLiteral>()
            .register_type::<BtStatus>()
            .register_type::<BehaviorTreeStatus>()
            .register_type::<Perception>()
            .register_type::<HearingEvent>()
            .register_type::<Abilities>()
            .register_type::<Ability>()
            .register_type::<AbilityDef>()
            .register_type::<StateTreeStatus>()
            .register_type::<StateTreeInbox>()
            .register_type::<StateTreeRunStatus>()
            .register_type::<AiTickBudget>()
            .register_type::<AiTickTier>()
            .register_type::<AiTickStats>()
            .register_type::<AiImportance>()
            .register_type::<LogicGraphStatus>()
            .register_type::<logic::LogicRuleStatus>()
            .register_type::<logic::LogicLocalStatus>()
            .register_type::<logic::LogicWorldBlackboards>()
            .register_type::<LogicInbox>()
            // Logic authored on prefab and level entities (P-165, ADR-8566).
            .register_type::<logic::LogicGraphRef>()
            .register_type::<logic::LogicLocalOverride>()
            .register_type::<logic::LogicOverrideValue>()
            .init_resource::<logic::LogicGraphRefLoads>()
            .add_systems(PreUpdate, logic::attach_logic_graph_refs)
            .init_resource::<BtTaskRegistry>()
            .init_resource::<state_tree::StateTreeReloads>()
            .init_resource::<logic::LogicReloads>()
            .init_resource::<debug_gizmos::AiDebugDraw>()
            .add_message::<regia_asset::SourceAssetChanged>()
            .add_systems(Update, state_tree::watch_state_tree_sources)
            .add_message::<HearingEvent>()
            .register_type::<path_provider::MoveToStatus>()
            .register_type::<path_provider::MoveToReport>()
            .register_type::<path_provider::MoveToState>()
            .configure_sets(FixedUpdate, AiStep.after(regia_physics::PhysicsStep))
            .add_systems(
                FixedUpdate,
                (
                    abilities::tick_abilities_with,
                    run_behavior_trees,
                    state_tree::run_state_trees,
                    logic::run_logic_graphs,
                )
                    .chain()
                    .in_set(AiStep),
            );
        // The `MoveTo` seam (ADR-8293): a `navmesh` provider unless the game installed its own set,
        // and the blackboard-driven leaf in the palette. No new system: the behavior-tree pass
        // refills the providers' budgets before the first tree ticks.
        if !app.world().contains_resource::<path_provider::PathProviders>() {
            app.insert_resource(path_provider::PathProviders::with_defaults());
        }
        path_provider::register_move_to(&mut app.world_mut().resource_mut::<BtTaskRegistry>());
        #[cfg(feature = "debug_draw")]
        app.add_systems(
            PostUpdate,
            perception::draw_perception
                .run_if(regia_render::gizmos::gizmo_group_enabled::<regia_render::gizmos::AiPerceptionGizmos>)
                .after(PostUpdateSet::TransformPropagate),
        );
        #[cfg(feature = "debug_draw")]
        app.add_systems(
            PostUpdate,
            formation::draw_formation_gizmos
                .run_if(regia_render::gizmos::gizmo_group_enabled::<regia_render::gizmos::FormationGizmos>)
                .after(PostUpdateSet::TransformPropagate),
        );
        #[cfg(feature = "debug_draw")]
        debug_gizmos::register_draw(app);
    }
}

/// The fixed-step behaviour-tree tick of [`AiPlugin`]. The tick is an exclusive system, so if it were
/// left unordered the executor would pick where it runs among the other fixed-step systems, and that
/// place could change with the thread count or with an unrelated plugin. [`AiPlugin`] runs it after
/// `PhysicsStep` (ADR-1892): an impulse a leaf writes lands on the next step, every time. Order it
/// against the game's other steps, for example `(AiStep, NavStep).chain()` when trees plan nav paths
/// (ADR-537, ADR-867).
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AiStep;

/// Sorts agents into tick order: the order their tree component `C` arrived in the world
/// (`regia_ecs::spawn_order`, ADR-1347), not query (archetype) order and not entity-id order.
/// The take-run-reinsert ticks move agents between archetypes, and leaf tasks can race for the
/// same thing (`claim_slot` is first-wins), so an archetype history that differs between two runs
/// must not change who ticks first (ADR-1660). Entity ids are no better: `Commands::spawn`
/// reserves them in thread-timing order, and a late agent can reuse a freed low index (ADR-2396).
/// Without the [`SpawnOrder`](regia_ecs::spawn_order::SpawnOrder) resource the key falls back to
/// the entity id.
pub(crate) fn agent_order<C: Component>(world: &World, entities: &mut [Entity]) {
    agent_order_by(
        world.get_resource::<regia_ecs::spawn_order::SpawnOrder<C>>(),
        entities,
    );
}

/// [`agent_order`] for a system that holds the order as a parameter instead of the whole world.
pub(crate) fn agent_order_by<C: Component>(
    order: Option<&regia_ecs::spawn_order::SpawnOrder<C>>,
    entities: &mut [Entity],
) {
    match order {
        Some(order) => entities.sort_unstable_by_key(|e| order.key(*e)),
        None => entities.sort_unstable_by_key(|e| (u64::MAX, e.index().index(), e.to_bits())),
    }
}

/// `FixedUpdate` (exclusive): ticks every [`BehaviorTree`] with take-run-reinsert semantics, so a
/// leaf task's [`BtContext`] holds the world exclusively (the same pattern scripts use). After each
/// tick it writes a reflected [`BehaviorTreeStatus`] snapshot so the editor and agent protocol can
/// inspect the running tree.
fn run_behavior_trees(
    world: &mut World,
    query: &mut QueryState<Entity, (With<BehaviorTree>, Without<LayerParked>)>,
    mut entities: Local<Vec<Entity>>,
) {
    path_provider::begin_path_tick(world);
    tick_behavior_trees_with(world, query, &mut entities);
}

/// One pass of [`run_behavior_trees`] with its own query and list, for a test that drives it by
/// hand.
#[cfg(test)]
fn tick_behavior_trees(world: &mut World) {
    let mut query = world.query_filtered::<Entity, (With<BehaviorTree>, Without<LayerParked>)>();
    tick_behavior_trees_with(world, &mut query, &mut Vec::new());
}

#[tracing::instrument(skip_all, target = "regia_ai", name = "ai_tick")]
fn tick_behavior_trees_with(
    world: &mut World,
    query: &mut QueryState<Entity, (With<BehaviorTree>, Without<LayerParked>)>,
    entities: &mut Vec<Entity>,
) {
    // The fixed step, never the frame delta: trees tick once per fixed step, so a frame delta
    // would tie cooldowns and timeouts to the frame rate and make replays depend on frame timing
    // (ADR-537, ADR-867).
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |fixed| fixed.step().as_secs_f32());
    entities.clear();
    entities.extend(query.iter(world));
    agent_order::<BehaviorTree>(world, entities);
    // One lookup per pass: with no debugger attached the loop below does nothing extra.
    let debugging = world.contains_resource::<BtDebugger>();
    // An `AiTickBudget`, when one is on, leaves in `entities` the trees of this step and gives each its
    // accumulated time (ADR-2709). Otherwise the list stays as it is and is empty.
    let budget_dts = tick_budget::plan_behavior_trees(world, entities, dt);
    for (index, &entity) in entities.iter().enumerate() {
        // An earlier leaf may have despawned this agent.
        let Ok(mut agent) = world.get_entity_mut(entity) else {
            continue;
        };
        let Some(mut tree) = agent.take::<BehaviorTree>() else {
            continue;
        };
        let dt = budget_dts.get(index).copied().unwrap_or(dt);
        let mut ctx = BtContext { world, entity, dt };
        tree.tick(&mut ctx);
        if debugging && let Some(mut debugger) = world.get_resource_mut::<BtDebugger>() {
            debugger.record(entity, &tree);
        }
        // A leaf may have despawned its own agent: drop the tree instead of re-inserting. A leaf
        // that installed a new tree on its own agent keeps it: the old tree is dropped without
        // `reset` (ADR-2396), so its spatial queries go with it.
        let dropped = if let Ok(mut agent) = world.get_entity_mut(entity) {
            let keep_installed = agent.contains::<BehaviorTree>();
            // `get_mut` marks `Changed` even when the bytes match, so read first and skip the write
            // on a steady tick.
            let unchanged = agent.get::<BehaviorTreeStatus>().is_some_and(|status| {
                status.last_status == tree.last_status
                    && status.node_status.as_slice() == tree.node_statuses()
            });
            if !unchanged {
                if let Some(mut status) = agent.get_mut::<BehaviorTreeStatus>() {
                    status.last_status = tree.last_status;
                    let next = tree.node_statuses();
                    if status.node_status.len() == next.len() {
                        status.node_status.copy_from_slice(next);
                    } else {
                        status.node_status.clear();
                        status.node_status.extend_from_slice(next);
                    }
                } else {
                    agent.insert(BehaviorTreeStatus {
                        last_status: tree.last_status,
                        node_status: tree.node_statuses().to_vec(),
                    });
                }
            }
            if keep_installed {
                true
            } else {
                agent.insert(tree);
                false
            }
        } else {
            true
        };
        if dropped {
            forget_spatial_queries(world, entity);
        }
    }
    tick_budget::recycle_dts(world, budget_dts);
}

/// A tree driver found its agent despawned mid-tick: the tree is dropped without `reset`, so drop
/// the agent's spatial queries (in flight and finished) here, or their results are kept forever.
pub(crate) fn forget_spatial_queries(world: &mut World, owner: Entity) {
    if let Some(mut manager) = world.get_resource_mut::<spatial_query::SpatialQueryManager>() {
        manager.forget_owner(owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blackboard::Blackboard;
    use crate::task::{BtContext, BtStatus, BtTask, BtTaskRegistration};

    #[derive(Default)]
    struct AlwaysSuccess;
    impl BtTask for AlwaysSuccess {
        fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for AlwaysSuccess {
        fn construct() -> Box<dyn BtTask> {
            Box::new(AlwaysSuccess)
        }
        fn task_type_path() -> &'static str {
            "test::AlwaysSuccess"
        }
    }

    #[derive(Resource, Default)]
    struct SeenDt(Vec<f32>);

    #[derive(Default)]
    struct RecordDt;
    impl BtTask for RecordDt {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let dt = ctx.dt;
            ctx.world.resource_mut::<SeenDt>().0.push(dt);
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for RecordDt {
        fn construct() -> Box<dyn BtTask> {
            Box::new(RecordDt)
        }
        fn task_type_path() -> &'static str {
            "test::RecordDt"
        }
    }

    #[derive(Default)]
    struct DespawnSelf;
    impl BtTask for DespawnSelf {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let entity = ctx.entity;
            ctx.world.despawn(entity);
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for DespawnSelf {
        fn construct() -> Box<dyn BtTask> {
            Box::new(DespawnSelf)
        }
        fn task_type_path() -> &'static str {
            "test::DespawnSelf"
        }
    }

    #[test]
    fn tick_uses_the_fixed_step_not_the_frame_delta() {
        let mut world = World::new();
        world.insert_resource(Time::default());
        // `FixedTime` has private fields, so set the rate on a default value.
        let mut fixed = FixedTime::default();
        fixed.hz = 50.0;
        world.insert_resource(fixed);
        world.init_resource::<SeenDt>();
        let mut registry = BtTaskRegistry::default();
        registry.register::<RecordDt>();
        let tree = BehaviorTree::compile(&BehaviorTreeDef::leaf("l", "test::RecordDt"), &registry)
            .expect("compiles");
        world.spawn(tree);
        tick_behavior_trees(&mut world);
        let seen = &world.resource::<SeenDt>().0;
        assert_eq!(seen.len(), 1);
        assert!((seen[0] - 0.02).abs() < 1e-6, "dt was {}", seen[0]);
    }

    #[test]
    fn a_parked_agent_does_not_tick() {
        let mut world = World::new();
        world.insert_resource(Time::default());
        world.insert_resource(FixedTime::default());
        world.init_resource::<SeenDt>();
        let mut registry = BtTaskRegistry::default();
        registry.register::<RecordDt>();
        let tree = BehaviorTree::compile(&BehaviorTreeDef::leaf("l", "test::RecordDt"), &registry)
            .expect("compiles");
        let agent = world
            .spawn((
                tree,
                LayerParked {
                    restore: regia_scene::prelude::Visibility::Inherited,
                    inherited: false,
                },
            ))
            .id();
        tick_behavior_trees(&mut world);
        assert!(world.resource::<SeenDt>().0.is_empty());
        assert!(world.get::<BehaviorTreeStatus>(agent).is_none());

        world.entity_mut(agent).remove::<LayerParked>();
        tick_behavior_trees(&mut world);
        assert_eq!(world.resource::<SeenDt>().0.len(), 1);
    }

    #[test]
    fn a_leaf_that_despawns_its_agent_does_not_abort_the_tick() {
        let mut world = World::new();
        world.insert_resource(Time::default());
        world.insert_resource(FixedTime::default());
        let mut registry = BtTaskRegistry::default();
        registry.register::<DespawnSelf>();
        let def = BehaviorTreeDef::leaf("l", "test::DespawnSelf");
        let a = world
            .spawn(BehaviorTree::compile(&def, &registry).expect("compiles"))
            .id();
        let b = world
            .spawn(BehaviorTree::compile(&def, &registry).expect("compiles"))
            .id();
        tick_behavior_trees(&mut world);
        assert!(world.get_entity(a).is_err());
        assert!(world.get_entity(b).is_err());
    }

    #[test]
    fn ai_plugin_ticks_trees_and_writes_a_reflected_status() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin));
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<AlwaysSuccess>();
        let def = BehaviorTreeDef::leaf("ok", "test::AlwaysSuccess");
        let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
        let entity = app.world_mut().spawn((Blackboard::default(), tree)).id();

        for _ in 0..3 {
            app.update();
        }

        let status = app
            .world()
            .get::<BehaviorTreeStatus>(entity)
            .expect("a reflected status snapshot was written each tick");
        assert_eq!(status.last_status, BtStatus::Success);
        assert_eq!(status.node_status, vec![BtStatus::Success]);
    }

    /// Counts its ticks and records the tick delta on the blackboard.
    #[derive(Default)]
    struct Probe;
    impl BtTask for Probe {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let dt = ctx.dt;
            if let Some(mut bb) = ctx.blackboard_mut() {
                let ticks = bb.get_int("ticks") + 1;
                bb.insert("ticks", ticks);
                bb.insert("last_dt", dt);
            }
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for Probe {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Probe)
        }
        fn task_type_path() -> &'static str {
            "test::Probe"
        }
    }

    /// Despawns the entity named by the blackboard key `victim` (possibly itself).
    #[derive(Default)]
    struct Despawner;
    impl BtTask for Despawner {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let victim = ctx.blackboard().map(|bb| bb.get_entity("victim"));
            if let Some(victim) = victim {
                let _ = ctx.world.despawn(victim);
            }
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for Despawner {
        fn construct() -> Box<dyn BtTask> {
            Box::new(Despawner)
        }
        fn task_type_path() -> &'static str {
            "test::Despawner"
        }
    }

    #[test]
    fn each_fixed_substep_ticks_with_the_step_length() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin));
        // One frame is 1/30 s and the fixed step 1/60 s: two substeps per frame.
        regia_app::enable_fixed_step(&mut app, 30.0);
        {
            let mut fixed = app.world_mut().resource_mut::<FixedTime>();
            fixed.hz = 60.0;
            fixed.max_substeps = 5;
        }
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<Probe>();
        let def = BehaviorTreeDef::leaf("probe", "test::Probe");
        let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
        let entity = app.world_mut().spawn((Blackboard::default(), tree)).id();
        for _ in 0..4 {
            app.update();
        }
        let bb = app.world().get::<Blackboard>(entity).expect("blackboard");
        assert!(bb.get_int("ticks") >= 2, "substeps ran");
        assert_eq!(bb.get_float("last_dt"), (1.0f64 / 60.0) as f32);
    }

    #[test]
    fn a_leaf_that_despawns_agents_does_not_panic() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin));
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<Despawner>();
        let def = BehaviorTreeDef::leaf("die", "test::Despawner");
        let compile = |app: &App| {
            BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles")
        };
        let (t1, t2) = (compile(&app), compile(&app));
        let first = app.world_mut().spawn((Blackboard::default(), t1)).id();
        // `first` despawns itself; `second` despawns `first`. Whichever ticks first, the other
        // finds its agent gone (or the tree's entity gone) and must not panic.
        app.world_mut()
            .get_mut::<Blackboard>(first)
            .expect("blackboard")
            .insert("victim", first);
        let mut second_bb = Blackboard::default();
        second_bb.insert("victim", first);
        let second = app.world_mut().spawn((second_bb, t2)).id();
        app.update();
        assert!(app.world().get_entity(first).is_err());
        assert!(app.world().get_entity(second).is_ok());
    }

    /// Records the `dt` each tick receives and, optionally, despawns its agent.
    #[derive(Default)]
    struct DtProbe;
    impl BtTask for DtProbe {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let dt = ctx.dt;
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.values
                    .insert("dt".to_string(), crate::blackboard::BbValue::Float(dt));
            }
            BtStatus::Running
        }
    }
    impl BtTaskRegistration for DtProbe {
        fn construct() -> Box<dyn BtTask> {
            Box::new(DtProbe)
        }
        fn task_type_path() -> &'static str {
            "test::DtProbe"
        }
    }

    #[derive(Default)]
    struct DespawnOwnAgent;
    impl BtTask for DespawnOwnAgent {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            ctx.world.despawn(ctx.entity);
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for DespawnOwnAgent {
        fn construct() -> Box<dyn BtTask> {
            Box::new(DespawnOwnAgent)
        }
        fn task_type_path() -> &'static str {
            "test::DespawnOwnAgent"
        }
    }

    #[test]
    fn tick_receives_the_fixed_step_not_the_frame_delta() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin));
        // 20 fps frames, 60 Hz fixed steps: three steps per frame, each one 1/60 s.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 20.0),
        ));
        {
            let mut fixed = app.world_mut().resource_mut::<FixedTime>();
            fixed.hz = 60.0;
            fixed.max_substeps = 8;
        }
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<DtProbe>();
        let def = BehaviorTreeDef::leaf("probe", "test::DtProbe");
        let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
        let entity = app.world_mut().spawn((Blackboard::default(), tree)).id();
        for _ in 0..3 {
            app.update();
        }
        let bb = app.world().get::<Blackboard>(entity).expect("blackboard");
        let Some(crate::blackboard::BbValue::Float(dt)) = bb.values.get("dt") else {
            panic!("the probe leaf never ticked");
        };
        assert!((dt - 1.0 / 60.0).abs() < 1e-5, "dt was {dt}");
    }

    #[test]
    fn a_leaf_that_despawns_its_agent_does_not_panic() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin));
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<DespawnOwnAgent>();
        let def = BehaviorTreeDef::leaf("die", "test::DespawnOwnAgent");
        let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
        let entity = app.world_mut().spawn((Blackboard::default(), tree)).id();
        for _ in 0..3 {
            app.update();
        }
        assert!(app.world().get_entity(entity).is_err());
    }

    /// A leaf that installs a replacement tree on the same entity drops the old tree without
    /// `reset`. The old owner's in-flight and finished spatial queries must go with it.
    #[derive(Resource)]
    struct AudfixSubmitted(crate::spatial_query::RequestId);

    #[derive(Default)]
    struct ReplaceTreeAndQuery;
    impl BtTask for ReplaceTreeAndQuery {
        fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
            let entity = ctx.entity;
            let id = ctx.world.resource_mut::<SpatialQueryManager>().submit(
                crate::spatial_query::SpatialQueryRequest::new("no-such-template", entity),
            );
            ctx.world.insert_resource(AudfixSubmitted(id));
            let mut registry = BtTaskRegistry::default();
            registry.register::<AlwaysSuccess>();
            let next =
                BehaviorTree::compile(&BehaviorTreeDef::leaf("next", "test::AlwaysSuccess"), &registry)
                    .expect("the replacement tree compiles");
            ctx.world.entity_mut(entity).insert(next);
            BtStatus::Success
        }
    }
    impl BtTaskRegistration for ReplaceTreeAndQuery {
        fn construct() -> Box<dyn BtTask> {
            Box::new(ReplaceTreeAndQuery)
        }
        fn task_type_path() -> &'static str {
            "test::ReplaceTreeAndQuery"
        }
    }

    #[test]
    fn audfix_replacing_a_behavior_tree_forgets_its_spatial_queries() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AiPlugin, SpatialQueryPlugin));
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register::<ReplaceTreeAndQuery>();
        let def = BehaviorTreeDef::leaf("q", "test::ReplaceTreeAndQuery");
        let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
        let entity = app.world_mut().spawn((Blackboard::default(), tree)).id();
        tick_behavior_trees(app.world_mut());
        assert!(
            app.world().get::<BehaviorTree>(entity).is_some(),
            "the replacement tree stays on the agent"
        );
        let id = app.world().resource::<AudfixSubmitted>().0;
        let manager = app.world().resource::<SpatialQueryManager>();
        assert!(
            !manager.is_active(id) && !manager.is_finished(id),
            "active={} finished={} (replacing the tree must drop the old owner's queries)",
            manager.is_active(id),
            manager.is_finished(id)
        );
    }
}
