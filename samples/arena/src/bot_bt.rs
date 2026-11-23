// SPDX-License-Identifier: MIT-0
//! The behaviour tree that drives a navmesh bot (`docs/specs/22-tier2.md` § T2.2): two leaf tasks
//! (seek + fire) composed into a parallel node, attached to each `AiController` pawn when
//! `ArenaConfig.use_navmesh` is on.
//!
//! The `SeekEnemy` leaf picks the nearest enemy, writes the target into the `Blackboard` and plans a
//! navmesh path into the `NavAgentState` (which `regia_nav::NavPlugin` then follows with ORCA
//! avoidance). The `FireAtEnemy` leaf fires when a line of sight is clear, on a cooldown. Both run
//! every tick under a `Parallel`, so movement and shooting happen together.

use regia_ai::{BehaviorTreeBuilder, BehaviorTreeDef, BtContext, BtStatus, BtTask, BtTaskRegistration};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_nav::{NavAgentState, NavMesh, NavObstacles, NavQuery};
use regia_physics::queries::{QueryFilter, WorldQueries};
use regia_scene::Transform;

use crate::pawn::Pawn;
use crate::weapons::{FireRequest, Weapon};

/// Maximum distance a bot will engage an enemy (m).
const ENGAGE_RANGE: f32 = 30.0;
/// Firing cooldown (s).
const FIRE_COOLDOWN: f32 = 0.6;
/// Every Nth shot is a rocket; the rest are hitscan (deterministic, no RNG).
const ROCKET_EVERY: u32 = 4;

/// Registers the arena's leaf tasks into the [`regia_ai::BtTaskRegistry`].
pub fn register_bot_tasks(world: &mut World) {
    let mut registry = world.get_resource_or_insert_with::<regia_ai::BtTaskRegistry>(Default::default);
    registry.register::<SeekEnemy>();
    registry.register::<FireAtEnemy>();
}

/// Builds the shared bot behaviour tree (a parallel of seek + fire).
pub fn build_bot_tree() -> BehaviorTreeDef {
    let mut builder = BehaviorTreeBuilder::new();
    let seek = builder.leaf("seek", <SeekEnemy as BtTaskRegistration>::task_type_path());
    let fire = builder.leaf("fire", <FireAtEnemy as BtTaskRegistration>::task_type_path());
    let root = builder.parallel("bot", regia_ai::ParallelPolicy::RequireAll, vec![seek, fire]);
    builder.build(root)
}

/// Whether the match is in the combat phase (bots act only during the round).
fn in_round(world: &World) -> bool {
    crate::match_flow::arena_state(world).is_some_and(|s| s.phase == crate::match_flow::Phase::Round)
}

/// Compiles a bot tree from the shared definition.
pub fn compile_bot_tree(world: &World, def: &BehaviorTreeDef) -> regia_ai::BehaviorTree {
    let registry = world.resource::<regia_ai::BtTaskRegistry>();
    regia_ai::BehaviorTree::compile(def, registry).expect("bot tasks are registered")
}

/// Leaf: pick the nearest alive enemy, record it in the blackboard, and plan a navmesh path to it.
#[derive(Default, regia_ai::BtTask)]
struct SeekEnemy;

impl BtTask for SeekEnemy {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if !in_round(ctx.world) {
            return BtStatus::Running;
        }
        let self_pos = ctx.position().unwrap_or(Vec3::ZERO);
        let own_slot = ctx
            .world
            .get::<Pawn>(ctx.entity)
            .map(|p| p.slot)
            .unwrap_or(u8::MAX);

        // The nearest alive enemy.
        let mut best: Option<(f32, Entity, Vec3)> = None;
        {
            let mut query = ctx.world.query::<(Entity, &Pawn, &Transform)>();
            for (entity, pawn, transform) in query.iter(ctx.world) {
                if pawn.slot == own_slot || entity == ctx.entity {
                    continue;
                }
                let alive = ctx
                    .world
                    .get::<regia_framework::damage::Health>(entity)
                    .is_none_or(|h| !h.is_dead());
                if !alive {
                    continue;
                }
                let dist = (transform.translation - self_pos).length();
                if dist <= ENGAGE_RANGE && best.is_none_or(|(d, _, _)| dist < d) {
                    best = Some((dist, entity, transform.translation));
                }
            }
        }

        let Some((_, target, target_pos)) = best else {
            if let Some(mut bb) = ctx.blackboard_mut() {
                bb.insert("has_target", false);
            }
            if let Some(mut state) = ctx.world.get_mut::<NavAgentState>(ctx.entity) {
                state.clear_path();
            }
            return BtStatus::Running;
        };

        // Record the target and plan a path to it.
        if let Some(mut bb) = ctx.blackboard_mut() {
            bb.insert("has_target", true);
            bb.insert("target", target);
            bb.insert("target_pos", target_pos);
        }
        plan_path_to(ctx, self_pos, target_pos);
        BtStatus::Running
    }
}

/// Leaf: fire at the blackboard target when a line of sight is clear, on a cooldown.
#[derive(Default, regia_ai::BtTask)]
struct FireAtEnemy {
    /// Remaining fire cooldown (s).
    cooldown: f32,
    /// Shots fired (every fourth is a rocket).
    shots: u32,
}

impl BtTask for FireAtEnemy {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        if !in_round(ctx.world) {
            return BtStatus::Running;
        }
        self.cooldown = (self.cooldown - ctx.dt).max(0.0);

        let target = ctx
            .blackboard()
            .and_then(|bb| (bb.get_bool("has_target")).then(|| bb.get_entity("target")))
            .filter(|e| *e != Entity::PLACEHOLDER);
        let target_pos = ctx
            .blackboard()
            .and_then(|bb| (bb.get_bool("has_target")).then(|| bb.get_vec3("target_pos")));

        let (Some(target), Some(target_pos)) = (target, target_pos) else {
            return BtStatus::Running;
        };
        let eye = ctx.position().unwrap_or(Vec3::ZERO);

        // Line of sight gate (same as the steering bots).
        let dir = (target_pos - eye).normalize_or_zero();
        let clear = line_of_sight_clear(ctx.world, eye, dir, ENGAGE_RANGE, ctx.entity)
            .is_some_and(|hit| hit.entity == target);

        if clear && self.cooldown <= 0.0 {
            self.shots = self.shots.saturating_add(1);
            let weapon = if self.shots.is_multiple_of(ROCKET_EVERY) {
                Weapon::Rocket
            } else {
                Weapon::Rifle
            };
            ctx.world.write_message(FireRequest {
                shooter: ctx.entity,
                position: eye,
                direction: dir,
                weapon,
            });
            self.cooldown = FIRE_COOLDOWN;
        }
        BtStatus::Running
    }
}

/// Plans a path from `from` to `to` and installs it on the agent's [`NavAgentState`].
fn plan_path_to(ctx: &mut BtContext<'_>, from: Vec3, to: Vec3) {
    let path = {
        let mesh = ctx.world.resource::<NavMesh>();
        let obstacles = ctx.world.get_resource::<NavObstacles>();
        NavQuery { mesh, obstacles }.find_path(from, to, Vec3::splat(2.0))
    };
    if let Some(path) = path
        && let Some(mut state) = ctx.world.get_mut::<NavAgentState>(ctx.entity)
    {
        state.set_path(path);
    }
}

/// Casts a line-of-sight ray from `eye`; returns the first hit (or `None` when clear).
pub(crate) fn line_of_sight_clear(
    world: &World,
    eye: Vec3,
    dir: Vec3,
    max_distance: f32,
    exclude: Entity,
) -> Option<regia_physics::queries::RaycastHit> {
    let physics = world.get_resource::<regia_physics::PhysicsWorld>()?;
    let has_layers = world.get_resource::<regia_physics::PhysicsLayers>().is_some();
    WorldQueries {
        world: physics,
        layers: has_layers.then_some(world.resource::<regia_physics::PhysicsLayers>()),
    }
    .raycast(
        eye,
        dir,
        max_distance,
        QueryFilter::new().exclude_entities(&[exclude]),
    )
}
