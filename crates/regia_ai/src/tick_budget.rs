// SPDX-License-Identifier: MPL-2.0
//! The AI tick budget (ADR-2709, `book/agents/ai-squads.md`): at most so many behavior trees, state
//! trees and logic graphs (spec 102 § 2, as `TreeKind::Logic`) tick per fixed step. Near agents tick every step, far ones at a reduced cadence with the time
//! they skipped added to their `dt`, and the ones that are due wait their turn in a stable order.
//!
//! Off by default. Without an [`AiTickBudget`] resource, or with `enabled: false`, the tree systems do
//! what they always did: one resource lookup is the whole cost, and no system is added to any schedule.
//!
//! Scheduling is counted in fixed steps, never timed, so two runs from the same state tick the same
//! trees on the same steps. Each step:
//!
//! 1. A tree is *due* when `age >= cadence`, where `age` is the steps since its last tick and `cadence`
//!    comes from the tier of its distance to the focus entity. A tree the scheduler has not seen yet is
//!    due at once, and an [`AiImportance::always_on`] agent is due every step.
//! 2. Due trees are taken in this order: trees overdue by a whole cadence, always-on agents, then the
//!    rest; within each, latest first (`age - cadence`), then by nearer tier, then in the order the
//!    scheduler first saw them, up to the tier's and the step's limits. The ones left over stay due, so
//!    they are later next step and go earlier: that is the round robin. The overdue class is what keeps
//!    always-on agents from starving everyone else when they alone fill the step's limit.
//! 3. A tree that ticks gets `dt = age * fixed step`, so a timer inside it runs at the same speed at any
//!    cadence, and cadence 1 sees exactly the fixed step.
//!
//! When the demand fits (the sum over tiers of `trees / cadence` is at most `max_trees_per_step`) and
//! each cadence divides the next, every tree ticks exactly on its cadence. When it does not fit, the
//! limits still hold, every tree still ticks, and [`AiTickStats`] counts how late they ran.
//!
//! A state tree that is paused or finished cannot do any work, so it uses no budget: it is visited every
//! step (its tick returns at once) and is not counted, and it comes back with one step of time.

use std::cmp::Reverse;
use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3};
use regia_scene::layers::LayerParked;
use regia_scene::{GlobalTransform, Transform};

use crate::logic::LogicGraph;
use crate::state_tree::{StateTree, StateTreeRunStatus};

/// One distance band of an [`AiTickBudget`].
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default)]
pub struct AiTickTier {
    /// The band ends at this distance from the focus, inclusive. The last tier also takes everything
    /// beyond it.
    pub max_distance: f32,
    /// A tree in this tier ticks every this many fixed steps (0 counts as 1).
    pub cadence_steps: u32,
    /// The most trees of this tier that tick in one step (0: no limit of its own).
    pub max_trees_per_step: u32,
}

impl Default for AiTickTier {
    fn default() -> Self {
        Self {
            max_distance: f32::MAX,
            cadence_steps: 1,
            max_trees_per_step: 0,
        }
    }
}

/// The tick budget's settings. Insert it (or call `ai.tick_budget_set`) and set `enabled` to turn the
/// budget on.
#[derive(Resource, Reflect, Debug, Clone, PartialEq)]
#[reflect(Resource, Default)]
pub struct AiTickBudget {
    /// Whether the budget applies. Off, every tree ticks every fixed step.
    pub enabled: bool,
    /// The most trees (behavior and state trees together) that tick in one fixed step (0: no limit).
    pub max_trees_per_step: u32,
    /// The tiers, nearest first. Empty: one tier with cadence 1.
    pub tiers: Vec<AiTickTier>,
    /// The entity distances are measured from, unless an agent's [`AiImportance`] names its own. With no
    /// focus, or one that is gone, every agent falls in the last tier.
    pub focus: Option<Entity>,
}

impl Default for AiTickBudget {
    fn default() -> Self {
        Self {
            enabled: false,
            max_trees_per_step: 64,
            tiers: vec![
                AiTickTier {
                    max_distance: 30.0,
                    cadence_steps: 1,
                    max_trees_per_step: 0,
                },
                AiTickTier {
                    max_distance: 80.0,
                    cadence_steps: 4,
                    max_trees_per_step: 0,
                },
                AiTickTier {
                    max_distance: f32::MAX,
                    cadence_steps: 16,
                    max_trees_per_step: 0,
                },
            ],
            focus: None,
        }
    }
}

/// How much one agent matters to the budget. An agent without it is scheduled by its distance to the
/// budget's focus, like one with the default value.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct AiImportance {
    /// Due every step, and taken before everything but a tree overdue by a whole cadence. Tier limits do
    /// not apply and it does not count against them; the step's limit does.
    pub always_on: bool,
    /// Measure this agent's distance from this entity instead of the budget's focus.
    pub focus: Option<Entity>,
}

/// What the budget did, counted. Written while the budget is on.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct AiTickStats {
    /// Fixed steps scheduled.
    pub steps: u64,
    /// Trees the scheduler knows about.
    pub trees: u32,
    /// Trees that ticked in the last step the budget ran (0 once it is off).
    pub ticks_last_step: u32,
    /// Trees that were due in the last step the budget ran and had to wait (0 once it is off).
    pub deferred_last_step: u32,
    /// Trees ticked by the budget since the app started.
    pub ticks_total: u64,
    /// Ticks that ran later than their cadence.
    pub late_ticks_total: u64,
    /// The latest any due tree has been, ticked or still waiting, in steps past its cadence.
    pub max_late_steps: u64,
    /// Trees per tier in the last step, nearest first.
    pub tier_trees: Vec<u32>,
    /// Ticks per tier in the last step, always-on agents apart, nearest first.
    pub tier_ticks: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TreeKind {
    Behavior,
    State,
    Logic,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// The order the scheduler first saw this tree.
    seq: u64,
    last_tick: u64,
    seen: u64,
    ticked: bool,
}

#[derive(Debug, Clone, Copy)]
struct Row {
    entity: Entity,
    kind: TreeKind,
    seq: u64,
    tier: usize,
    always_on: bool,
    overdue: bool,
    age: u64,
    lateness: u64,
}

/// The scheduler's memory. Present only while a budget is on.
#[derive(Resource)]
pub(crate) struct AiTickSchedule {
    /// A copy of the settings for the pass, kept so that a steady step allocates nothing.
    budget: Option<AiTickBudget>,
    step: u64,
    next_seq: u64,
    slots: HashMap<(Entity, TreeKind), Slot>,
    state_query: QueryState<Entity, (With<StateTree>, Without<LayerParked>)>,
    state_entities: Vec<Entity>,
    logic_query: QueryState<Entity, (With<LogicGraph>, Without<LayerParked>)>,
    logic_entities: Vec<Entity>,
    rows: Vec<Row>,
    tier_taken: Vec<u32>,
    behavior_plan: Vec<(u64, Entity, f32)>,
    state_plan: Vec<(u64, Entity, f32)>,
    logic_plan: Vec<(u64, Entity, f32)>,
    /// The state-tree pass of this step has not taken its plan yet.
    state_plan_ready: bool,
    /// The logic-graph pass of this step has not taken its plan yet.
    logic_plan_ready: bool,
    spare_dts: Vec<Vec<f32>>,
}

/// The world position and forward of `entity`, from the local `Transform`s up its `ChildOf` chain. The
/// `GlobalTransform` is propagated once per frame, so reading it on a later fixed step of a frame would
/// tie the answer to the frame rate; it serves an entity or ancestor with no `Transform`.
pub(crate) fn world_pose(world: &World, entity: Entity) -> Option<(Vec3, Vec3)> {
    let (transform, global, link) = (
        world.get::<Transform>(entity),
        world.get::<GlobalTransform>(entity),
        world.get::<ChildOf>(entity),
    );
    pose_from_chain(transform, global, link, |e| {
        (
            world.get::<Transform>(e),
            world.get::<GlobalTransform>(e),
            world.get::<ChildOf>(e),
        )
    })
}

/// The world pose of `entity` as a full affine, resolved by the same chain walk as
/// [`pose_from_chain`] (local `Transform`s up the `ChildOf` links, `GlobalTransform` only as the
/// fallback for an entity or ancestor without one). Tests that need more than the position and
/// forward — a zone's volume transform, for one — use this so the answer never comes from the
/// once-per-frame `GlobalTransform` snapshot.
pub(crate) fn affine_from_chain<'a>(
    transform: Option<&'a Transform>,
    global: Option<&'a GlobalTransform>,
    link: Option<&'a ChildOf>,
    mut get: impl FnMut(
        Entity,
    ) -> (
        Option<&'a Transform>,
        Option<&'a GlobalTransform>,
        Option<&'a ChildOf>,
    ),
) -> Option<Affine3A> {
    let Some(transform) = transform else {
        return global.map(|g| g.0);
    };
    let Some(link) = link else {
        return Some(transform.compute_affine());
    };
    let mut affine = transform.compute_affine();
    let mut next = link.parent();
    // The walk ends at the root; the bound only guards a malformed hierarchy.
    for _ in 0..64 {
        let (parent, parent_global, parent_link) = get(next);
        let Some(parent) = parent else {
            if let Some(parent_global) = parent_global {
                affine = parent_global.0 * affine;
            }
            break;
        };
        affine = parent.compute_affine() * affine;
        let Some(parent_link) = parent_link else {
            break;
        };
        next = parent_link.parent();
    }
    Some(affine)
}

/// The position and forward of [`affine_from_chain`]'s walk, as the query-based systems
/// (perception, the smart-object index) need them: the entity's own local `Transform`, then each
/// ancestor's up the `ChildOf` links; a missing `Transform` at any level falls back to that
/// entity's `GlobalTransform`. `get` reads one entity's three optional components.
pub(crate) fn pose_from_chain<'a>(
    transform: Option<&'a Transform>,
    global: Option<&'a GlobalTransform>,
    link: Option<&'a ChildOf>,
    mut get: impl FnMut(
        Entity,
    ) -> (
        Option<&'a Transform>,
        Option<&'a GlobalTransform>,
        Option<&'a ChildOf>,
    ),
) -> Option<(Vec3, Vec3)> {
    let Some(transform) = transform else {
        return global.map(|g| (g.translation(), g.0.transform_vector3(Vec3::NEG_Z)));
    };
    let Some(link) = link else {
        return Some((transform.translation, transform.rotation * Vec3::NEG_Z));
    };
    let mut affine = transform.compute_affine();
    let mut next = link.parent();
    // The walk ends at the root; the bound only guards a malformed hierarchy.
    for _ in 0..64 {
        let (parent, parent_global, parent_link) = get(next);
        let Some(parent) = parent else {
            if let Some(parent_global) = parent_global {
                affine = parent_global.0 * affine;
            }
            break;
        };
        affine = parent.compute_affine() * affine;
        let Some(parent_link) = parent_link else {
            break;
        };
        next = parent_link.parent();
    }
    Some((
        Vec3::from(affine.translation),
        affine.transform_vector3(Vec3::NEG_Z),
    ))
}

/// The tier of an agent at `position`, and its distance from the focus when there is one.
fn tier_of(
    world: &World,
    budget: &AiTickBudget,
    entity: Entity,
    default_focus: Option<Vec3>,
    importance: Option<&AiImportance>,
) -> (usize, Option<f32>) {
    let last = budget.tiers.len().saturating_sub(1);
    let focus = match importance.and_then(|i| i.focus) {
        Some(own) => world_pose(world, own).map(|(p, _)| p),
        None => default_focus,
    };
    let (Some(focus), Some((position, _))) = (focus, world_pose(world, entity)) else {
        return (last, None);
    };
    let distance = (position - focus).length();
    let tier = budget
        .tiers
        .iter()
        .position(|t| distance <= t.max_distance)
        .unwrap_or(last);
    (tier, Some(distance))
}

impl AiTickSchedule {
    fn new(world: &mut World) -> Self {
        Self {
            budget: None,
            step: 0,
            next_seq: 0,
            slots: HashMap::new(),
            state_query: world.query_filtered(),
            state_entities: Vec::new(),
            logic_query: world.query_filtered(),
            logic_entities: Vec::new(),
            rows: Vec::new(),
            tier_taken: Vec::new(),
            behavior_plan: Vec::new(),
            state_plan: Vec::new(),
            logic_plan: Vec::new(),
            state_plan_ready: false,
            logic_plan_ready: false,
            spare_dts: Vec::new(),
        }
    }

    fn take_buffer(&mut self) -> Vec<f32> {
        let mut buffer = self.spare_dts.pop().unwrap_or_default();
        buffer.clear();
        buffer
    }

    fn add_rows(
        &mut self,
        world: &World,
        budget: &AiTickBudget,
        default_focus: Option<Vec3>,
        kind: TreeKind,
        entities: &[Entity],
        step_dt: f32,
    ) {
        let step = self.step;
        for &entity in entities {
            let next_seq = &mut self.next_seq;
            let slot = self.slots.entry((entity, kind)).or_insert_with(|| {
                let seq = *next_seq;
                *next_seq += 1;
                Slot {
                    seq,
                    last_tick: step.saturating_sub(1),
                    seen: step,
                    ticked: false,
                }
            });
            slot.seen = step;
            let importance = world.get::<AiImportance>(entity);
            // A paused or finished state tree cannot do any work: it costs no budget and keeps no clock.
            if kind == TreeKind::State
                && world.get::<StateTree>(entity).is_some_and(|tree| {
                    tree.is_paused()
                        || matches!(
                            tree.status(),
                            StateTreeRunStatus::Succeeded | StateTreeRunStatus::Failed
                        )
                })
            {
                slot.last_tick = step;
                slot.ticked = true;
                self.state_plan.push((slot.seq, entity, step_dt));
                continue;
            }
            // A paused logic graph likewise; a running one with a timer due by now is due whatever its
            // cadence, so timers fire on their fixed step (spec 102 § 13 P1.12).
            let mut timer_due = false;
            if kind == TreeKind::Logic
                && let Some(graph) = world.get::<LogicGraph>(entity)
            {
                if graph.is_paused() {
                    slot.last_tick = step;
                    slot.ticked = true;
                    self.logic_plan.push((slot.seq, entity, step_dt));
                    continue;
                }
                let age = step.saturating_sub(slot.last_tick);
                timer_due = graph.steps_until_timer().is_some_and(|n| age >= n);
            }
            let (tier, _) = tier_of(world, budget, entity, default_focus, importance);
            let always_on = importance.is_some_and(|i| i.always_on);
            // Always-on means cadence 1, so more of them than the step's limit take turns.
            let cadence = if always_on {
                1
            } else {
                budget
                    .tiers
                    .get(tier)
                    .map_or(1, |t| u64::from(t.cadence_steps.max(1)))
            };
            let age = step.saturating_sub(slot.last_tick);
            if let Some(count) = self.tier_taken.get_mut(tier + budget.tiers.len().max(1)) {
                *count += 1;
            }
            if !slot.ticked || age >= cadence || timer_due {
                let lateness = age.saturating_sub(cadence);
                self.rows.push(Row {
                    entity,
                    kind,
                    seq: slot.seq,
                    tier,
                    always_on,
                    overdue: !always_on && lateness >= cadence,
                    age,
                    lateness,
                });
            }
        }
    }

    /// Picks this step's trees. `behavior_trees` holds the behavior trees in arrival order.
    fn plan(&mut self, world: &mut World, budget: &AiTickBudget, behavior_trees: &[Entity], step_dt: f32) {
        self.step += 1;
        let step = self.step;
        self.rows.clear();
        self.behavior_plan.clear();
        self.state_plan.clear();
        self.logic_plan.clear();
        // `tier_taken` holds the limited ticks per tier, then the trees per tier.
        self.tier_taken.clear();
        self.tier_taken.resize(budget.tiers.len().max(1) * 2, 0);

        let mut state_entities = std::mem::take(&mut self.state_entities);
        state_entities.clear();
        state_entities.extend(self.state_query.iter(world));
        crate::agent_order::<StateTree>(world, &mut state_entities);

        let world_ref: &World = world;
        let default_focus = budget
            .focus
            .and_then(|focus| world_pose(world_ref, focus))
            .map(|(p, _)| p);
        self.add_rows(
            world_ref,
            budget,
            default_focus,
            TreeKind::Behavior,
            behavior_trees,
            step_dt,
        );
        self.add_rows(
            world_ref,
            budget,
            default_focus,
            TreeKind::State,
            &state_entities,
            step_dt,
        );
        self.state_entities = state_entities;

        let mut logic_entities = std::mem::take(&mut self.logic_entities);
        logic_entities.clear();
        logic_entities.extend(self.logic_query.iter(world));
        crate::agent_order::<LogicGraph>(world, &mut logic_entities);
        self.add_rows(
            world,
            budget,
            default_focus,
            TreeKind::Logic,
            &logic_entities,
            step_dt,
        );
        self.logic_entities = logic_entities;

        // A tree that was not there this step (despawned, parked) starts afresh if it comes back, so a
        // parked agent does not wake with the whole time it was parked as one `dt`.
        let trees = behavior_trees.len() + self.state_entities.len() + self.logic_entities.len();
        if self.slots.len() != trees {
            self.slots.retain(|_, slot| slot.seen == step);
        }

        self.rows.sort_unstable_by_key(|r| {
            let class = if r.overdue {
                0
            } else if r.always_on {
                1
            } else {
                2
            };
            (class, Reverse(r.lateness), r.tier, r.seq)
        });
        let cap = match budget.max_trees_per_step {
            0 => usize::MAX,
            n => n as usize,
        };
        let tier_count = budget.tiers.len().max(1);
        let (mut taken, mut deferred, mut late, mut max_late) = (0usize, 0u32, 0u64, 0u64);
        for row in &self.rows {
            max_late = max_late.max(row.lateness);
            let tier_limit = budget.tiers.get(row.tier).map_or(0, |t| t.max_trees_per_step);
            let tier_full = !row.always_on
                && tier_limit > 0
                && self.tier_taken.get(row.tier).is_some_and(|n| *n >= tier_limit);
            if taken >= cap || tier_full {
                deferred += 1;
                continue;
            }
            taken += 1;
            if !row.always_on
                && let Some(count) = self.tier_taken.get_mut(row.tier)
            {
                *count += 1;
            }
            if row.lateness > 0 {
                late += 1;
            }
            if let Some(slot) = self.slots.get_mut(&(row.entity, row.kind)) {
                slot.last_tick = step;
                slot.ticked = true;
            }
            let dt = row.age as f32 * step_dt;
            match row.kind {
                TreeKind::Behavior => self.behavior_plan.push((row.seq, row.entity, dt)),
                TreeKind::State => self.state_plan.push((row.seq, row.entity, dt)),
                TreeKind::Logic => self.logic_plan.push((row.seq, row.entity, dt)),
            }
        }
        // Inside one step, trees tick in the stable order.
        self.behavior_plan.sort_unstable_by_key(|(seq, _, _)| *seq);
        self.state_plan.sort_unstable_by_key(|(seq, _, _)| *seq);
        self.logic_plan.sort_unstable_by_key(|(seq, _, _)| *seq);
        self.state_plan_ready = true;
        self.logic_plan_ready = true;

        let (tier_ticks, tier_trees) = self.tier_taken.split_at(tier_count);
        let mut stats = world.get_resource_or_insert_with(AiTickStats::default);
        stats.steps += 1;
        stats.trees = u32::try_from(trees).unwrap_or(u32::MAX);
        stats.ticks_last_step = u32::try_from(taken).unwrap_or(u32::MAX);
        stats.deferred_last_step = deferred;
        stats.ticks_total += taken as u64;
        stats.late_ticks_total += late;
        stats.max_late_steps = stats.max_late_steps.max(max_late);
        stats.tier_ticks.clear();
        stats.tier_ticks.extend_from_slice(tier_ticks);
        stats.tier_trees.clear();
        stats.tier_trees.extend_from_slice(tier_trees);
    }

    fn fill(plan: &[(u64, Entity, f32)], entities: &mut Vec<Entity>, dts: &mut Vec<f32>) {
        entities.clear();
        entities.extend(plan.iter().map(|(_, entity, _)| *entity));
        dts.extend(plan.iter().map(|(_, _, dt)| *dt));
    }
}

/// The behavior-tree pass asks here first. With a budget on it plans the step, leaves in `entities` the
/// behavior trees to tick and returns their `dt`s, parallel to `entities`. With none on it changes
/// nothing and returns an empty list.
pub(crate) fn plan_behavior_trees(world: &mut World, entities: &mut Vec<Entity>, step_dt: f32) -> Vec<f32> {
    if !world.get_resource::<AiTickBudget>().is_some_and(|b| b.enabled) {
        // A budget turned off forgets its schedule: turned on again, it starts afresh.
        if world.contains_resource::<AiTickSchedule>() {
            world.remove_resource::<AiTickSchedule>();
            if let Some(mut stats) = world.get_resource_mut::<AiTickStats>() {
                stats.ticks_last_step = 0;
                stats.deferred_last_step = 0;
            }
        }
        return Vec::new();
    }
    let _span = tracing::debug_span!(target: "regia_ai", "ai_tick_plan").entered();
    if !world.contains_resource::<AiTickSchedule>() {
        let schedule = AiTickSchedule::new(world);
        world.insert_resource(schedule);
    }
    world.resource_scope(|world, mut schedule: Mut<AiTickSchedule>| {
        let mut budget = schedule.budget.take().unwrap_or_default();
        if let Some(current) = world.get_resource::<AiTickBudget>() {
            budget.enabled = current.enabled;
            budget.max_trees_per_step = current.max_trees_per_step;
            budget.focus = current.focus;
            budget.tiers.clear();
            budget.tiers.extend_from_slice(&current.tiers);
        }
        schedule.plan(world, &budget, entities, step_dt);
        schedule.budget = Some(budget);
        let mut dts = schedule.take_buffer();
        AiTickSchedule::fill(&schedule.behavior_plan, entities, &mut dts);
        dts
    })
}

/// The state-tree pass asks here first: like [`plan_behavior_trees`], reading the plan made for this
/// step. With no plan waiting (the budget went on after the behavior-tree pass) it changes nothing.
pub(crate) fn plan_state_trees(world: &mut World, entities: &mut Vec<Entity>) -> Vec<f32> {
    let Some(mut schedule) = world.get_resource_mut::<AiTickSchedule>() else {
        return Vec::new();
    };
    if !schedule.state_plan_ready {
        return Vec::new();
    }
    schedule.state_plan_ready = false;
    let mut dts = schedule.take_buffer();
    AiTickSchedule::fill(&schedule.state_plan, entities, &mut dts);
    dts
}

/// The logic-graph pass asks here first: like [`plan_state_trees`], reading the plan made for this
/// step.
pub(crate) fn plan_logic_graphs(world: &mut World, entities: &mut Vec<Entity>) -> Vec<f32> {
    let Some(mut schedule) = world.get_resource_mut::<AiTickSchedule>() else {
        return Vec::new();
    };
    if !schedule.logic_plan_ready {
        return Vec::new();
    }
    schedule.logic_plan_ready = false;
    let mut dts = schedule.take_buffer();
    AiTickSchedule::fill(&schedule.logic_plan, entities, &mut dts);
    dts
}

/// Hands a `dt` list back for the next step, so a steady step allocates nothing.
pub(crate) fn recycle_dts(world: &mut World, dts: Vec<f32>) {
    if dts.capacity() > 0
        && let Some(mut schedule) = world.get_resource_mut::<AiTickSchedule>()
    {
        schedule.spare_dts.push(dts);
    }
}

/// What the budget thinks of one agent now, for tools.
#[derive(Debug, Clone, PartialEq)]
pub struct AiTickAgentInfo {
    /// The tier the agent is in now.
    pub tier: usize,
    /// Its distance from its focus, when it and the focus have positions.
    pub distance: Option<f32>,
    /// The always-on flag.
    pub always_on: bool,
    /// Steps since its behavior tree last ticked (`None`: no tree known to the scheduler).
    pub behavior_tree_age: Option<u64>,
    /// Steps since its state tree last ticked (`None`: no tree known to the scheduler).
    pub state_tree_age: Option<u64>,
}

/// The budget's view of `entity`, or `None` with no budget resource. Changes nothing.
pub fn ai_tick_agent_info(world: &World, entity: Entity) -> Option<AiTickAgentInfo> {
    let budget = world.get_resource::<AiTickBudget>()?;
    let default_focus = budget
        .focus
        .and_then(|focus| world_pose(world, focus))
        .map(|(p, _)| p);
    let importance = world.get::<AiImportance>(entity);
    let (tier, distance) = tier_of(world, budget, entity, default_focus, importance);
    let schedule = world.get_resource::<AiTickSchedule>();
    let age = |kind| {
        schedule.and_then(|s| {
            s.slots
                .get(&(entity, kind))
                .map(|slot| s.step.saturating_sub(slot.last_tick))
        })
    };
    Some(AiTickAgentInfo {
        tier,
        distance,
        always_on: importance.is_some_and(|i| i.always_on),
        behavior_tree_age: age(TreeKind::Behavior),
        state_tree_age: age(TreeKind::State),
    })
}
