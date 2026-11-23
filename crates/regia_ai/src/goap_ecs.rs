// SPDX-License-Identifier: MPL-2.0
//! GOAP on entities (ADR-2565): a [`GoapAgent`] owns a shared [`Planner`], a [`FactTable`] over its
//! [`Blackboard`] and a [`Goal`]; [`GoapPlugin`] plans on the async compute pool and replans when the
//! world state invalidates the plan.
//!
//! # What runs when
//!
//! Each fixed step, in entity order, an agent whose blackboard changed (or who is waiting for a plan):
//!
//! 1. takes the result of its planner task, if the task's fixed latency is up;
//! 2. snapshots its blackboard through the [`FactTable`];
//! 3. advances past every step whose effects now hold (game code does the work: a behavior-tree leaf
//!    or a script reads [`GoapAgent::action_key`] and writes the effect's fact when it is done);
//! 4. checks what is left of the plan against the state ([`Plan::validate_remaining`]), and when a
//!    precondition no longer holds, or the goal moved, starts a new search;
//! 5. publishes [`GoapStatus`] and the current action.
//!
//! # Determinism
//!
//! A task finishes when the thread pool lets it, so taking a result "when it is ready" would make the
//! tick a plan lands on depend on the machine. A result is taken exactly
//! [`GoapSettings::plan_latency_ticks`] fixed steps after the search started. If the task has not
//! finished by then (it may be queued behind a long bake on a small pool), it is dropped and the same
//! search runs right there: the search is deterministic, so the plan is the same either way, and its
//! cost is bounded by the expansion budget ([`PlanOptions::max_nodes`]) while a wait on the task would
//! not be. Two runs replan on the same ticks and get the same plans.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::tasks::{AsyncComputePool, Task, block_on};
use regia_scene::layers::LayerParked;

use crate::AiStep;
use crate::blackboard::{BbValue, Blackboard};
use crate::goap::{FactTable, Goal, IntEffectKind, Plan, PlanError, PlanOptions, Planner, WorldState};
use crate::perception_system::PerceptionStep;

/// The default [`GoapAgent::action_key`].
pub const DEFAULT_ACTION_KEY: &str = "goap_action";

/// The expansion budget of a [`GoapAgent`] unless set otherwise: a search that takes more nodes than
/// this stops with [`PlanError::LimitReached`], so a planner task has a bounded cost.
pub const DEFAULT_MAX_NODES: usize = 2_000;

/// A search running on the pool.
struct PendingPlan {
    task: Task<Result<Plan, PlanError>>,
    ready_tick: u64,
    start: WorldState,
    goal: Goal,
    options: PlanOptions,
}

/// An entity that plans. Build the [`Planner`] once and share it between agents of a kind: its plan
/// cache is shared with it. Needs a [`Blackboard`] on the same entity.
///
/// Not reflected (a planner holds closures); [`GoapStatus`] is its reflected view and `ai.goap_plan`
/// shows the whole plan.
#[derive(Component)]
#[require(GoapStatus)]
pub struct GoapAgent {
    planner: Arc<Planner>,
    facts: FactTable,
    goal: Goal,
    /// Search limits of each request. [`PlanOptions::max_nodes`] is the expansion budget.
    pub options: PlanOptions,
    /// The blackboard key that holds the current action's name (`BbValue::String`); removed when
    /// there is no current action. Empty writes nothing.
    pub action_key: String,
    /// Search again on any change to the facts, not only when the plan stops being valid. Off by
    /// default: a cheaper route appearing is not worth a search per blackboard write.
    pub replan_on_any_change: bool,
    plan: Option<Plan>,
    next_step: usize,
    step_base: WorldState,
    last_state: Option<WorldState>,
    failed_for: Option<(WorldState, Goal)>,
    last_error: Option<PlanError>,
    pending: Option<PendingPlan>,
    needs_look: bool,
}

impl std::fmt::Debug for GoapAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoapAgent")
            .field("goal", &self.goal)
            .field("options", &self.options)
            .field("plan", &self.plan)
            .field("next_step", &self.next_step)
            .field("planning", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl GoapAgent {
    /// An agent over `planner`, reading its facts through `facts` and working toward `goal`. Its
    /// budget is [`DEFAULT_MAX_NODES`] nodes; set [`GoapAgent::options`] to change it.
    #[must_use]
    pub fn new(planner: Arc<Planner>, facts: FactTable, goal: Goal) -> Self {
        Self {
            planner,
            facts,
            goal,
            options: PlanOptions {
                max_nodes: DEFAULT_MAX_NODES,
                ..PlanOptions::default()
            },
            action_key: DEFAULT_ACTION_KEY.to_string(),
            replan_on_any_change: false,
            plan: None,
            next_step: 0,
            step_base: WorldState::new(),
            last_state: None,
            failed_for: None,
            last_error: None,
            pending: None,
            needs_look: true,
        }
    }

    /// The shared planner.
    #[must_use]
    pub fn planner(&self) -> &Arc<Planner> {
        &self.planner
    }

    /// The names the facts go by.
    #[must_use]
    pub fn facts(&self) -> &FactTable {
        &self.facts
    }

    /// The goal.
    #[must_use]
    pub fn goal(&self) -> &Goal {
        &self.goal
    }

    /// Works toward a new goal from the next fixed step on.
    pub fn set_goal(&mut self, goal: Goal) {
        self.goal = goal;
        self.needs_look = true;
    }

    /// Looks again next fixed step even if the blackboard did not change. Use it after raising the
    /// budget in [`GoapAgent::options`]: a search that failed is otherwise not repeated until the
    /// facts or the goal change, since it would fail the same way.
    pub fn retry(&mut self) {
        self.failed_for = None;
        self.needs_look = true;
    }

    /// The plan being followed.
    #[must_use]
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// Which step of [`GoapAgent::plan`] is current.
    #[must_use]
    pub const fn next_step(&self) -> usize {
        self.next_step
    }

    /// The name of the current action.
    #[must_use]
    pub fn current_action(&self) -> Option<&str> {
        let step = self.plan.as_ref()?.steps().get(self.next_step)?;
        Some(step.name.as_str())
    }

    /// The facts as last read.
    #[must_use]
    pub const fn last_state(&self) -> Option<&WorldState> {
        self.last_state.as_ref()
    }

    /// Whether a search is running.
    #[must_use]
    pub const fn is_planning(&self) -> bool {
        self.pending.is_some()
    }
}

/// Where a [`GoapAgent`] stands.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Default)]
pub enum GoapState {
    /// Nothing has happened yet.
    #[default]
    Idle,
    /// A search is running on the pool.
    Planning,
    /// Following a plan: `GoapStatus::steps[next_step]` is the current action.
    Executing,
    /// The goal holds.
    Satisfied,
    /// The last search failed: see `GoapStatus::error`. Not repeated until the facts or the goal
    /// change.
    Failed,
}

impl GoapState {
    /// The lowercase name the agent protocol shows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Planning => "planning",
            Self::Executing => "executing",
            Self::Satisfied => "satisfied",
            Self::Failed => "failed",
        }
    }
}

/// The reflected view of a [`GoapAgent`], rewritten when it changes.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct GoapStatus {
    /// Where the agent stands.
    pub state: GoapState,
    /// The action names of the plan being followed (empty without one).
    pub steps: Vec<String>,
    /// Index of the current action in [`GoapStatus::steps`].
    pub next_step: u32,
    /// The plan's total cost.
    pub total_cost: f32,
    /// Nodes the search for this plan expanded.
    pub nodes_expanded: u32,
    /// Searches started so far.
    pub plans_started: u32,
    /// Of those, how many replaced or followed an earlier plan or failure.
    pub replans: u32,
    /// Why the last search failed: `no_plan`, `limit_reached` or `invalid_action`; empty otherwise.
    pub error: String,
}

/// The snake_case name of a [`PlanError`], as in [`GoapStatus::error`].
#[must_use]
pub const fn plan_error_name(error: PlanError) -> &'static str {
    match error {
        PlanError::NoPlan => "no_plan",
        PlanError::LimitReached => "limit_reached",
        PlanError::InvalidAction => "invalid_action",
    }
}

/// Global GOAP settings.
#[derive(Resource, Reflect, Debug, Clone, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct GoapSettings {
    /// Fixed steps between starting a search and taking its result (at least 1). The fixed latency
    /// is what keeps planning deterministic (see the module docs).
    pub plan_latency_ticks: u32,
    /// The most searches started in one fixed step; the others wait for the next step, in entity
    /// order. `0` starts none.
    pub max_plans_per_tick: u32,
}

impl Default for GoapSettings {
    fn default() -> Self {
        Self {
            plan_latency_ticks: 1,
            max_plans_per_tick: 16,
        }
    }
}

/// Counters for profiling and the agent protocol.
#[derive(Resource, Reflect, Debug, Clone, Default, PartialEq, Eq)]
#[reflect(Resource, Default)]
pub struct GoapStats {
    /// Fixed steps run so far.
    pub tick: u64,
    /// Searches started in the last fixed step.
    pub plans_started_last_tick: u32,
    /// Searches started in all.
    pub plans_started_total: u64,
    /// Searches the pool had not finished at their landing step, which then ran on the fixed step's
    /// thread. The only number here that depends on the machine: a diagnostic (raise
    /// [`GoapSettings::plan_latency_ticks`] when it climbs), never to feed back into the game.
    pub searches_run_inline: u64,
}

/// `FixedUpdate` set of [`GoapPlugin`]: after `PerceptionStep`, so the plan sees this step's
/// perception on the blackboard, and before `AiStep`, so a tree reads this step's current action.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GoapStep;

/// Installs GOAP on entities: plans for every [`GoapAgent`] on the async compute pool. Opt-in (not
/// part of [`crate::AiPlugin`]).
#[derive(Default)]
pub struct GoapPlugin;

impl Plugin for GoapPlugin {
    fn build(&self, app: &mut App) {
        regia_ecs::spawn_order::track_spawn_order::<GoapAgent>(app.world_mut());
        app.register_type::<GoapStatus>()
            .register_type::<GoapState>()
            .register_type::<GoapSettings>()
            .register_type::<GoapStats>()
            .init_resource::<GoapSettings>()
            .init_resource::<GoapStats>()
            .configure_sets(FixedUpdate, GoapStep.after(PerceptionStep).before(AiStep))
            .add_systems(FixedUpdate, goap_tick.in_set(GoapStep));
    }
}

impl GoapAgent {
    /// Whether every effect of step `index` holds in `state`, measured from the state the step
    /// started in (an `Add` effect only makes sense relative to that).
    fn step_done(&self, plan: &Plan, index: usize, state: &WorldState) -> bool {
        let Some(action) = plan
            .steps()
            .get(index)
            .and_then(|step| self.planner.actions().get(step.action_index))
        else {
            return false;
        };
        let expected = action.apply(&self.step_base);
        action.effects.is_satisfied_by(state)
            && action.int_effects.iter().all(|effect| {
                let fact = usize::from(effect.fact);
                let (now, wanted) = (state.get_int(fact), expected.get_int(fact));
                match effect.kind {
                    // A count that went past what the step adds has still got there: the game may
                    // gather more than the plan asked, or another system may add to the same fact.
                    IntEffectKind::Add(delta) if delta > 0 => now >= wanted,
                    IntEffectKind::Add(delta) if delta < 0 => now <= wanted,
                    _ => now == wanted,
                }
            })
    }

    /// Steps 3 and 4 of the module docs: the plan against the state. Returns whether a search is
    /// wanted.
    fn look(&mut self, state: &WorldState) -> bool {
        let changed = self.last_state.as_ref() != Some(state);
        self.last_state = Some(*state);

        if self.goal.is_satisfied_by(state) {
            self.plan = None;
            self.next_step = 0;
            return false;
        }
        if let Some(plan) = self.plan.take() {
            // Past the steps that are done; a later step starts from what is true now.
            while self.step_done(&plan, self.next_step, state) {
                self.next_step += 1;
                self.step_base = *state;
            }
            let valid = *plan.goal() == self.goal
                && plan.validate_remaining(state, self.next_step).is_valid()
                && self.next_step < plan.len();
            if valid && !(self.replan_on_any_change && changed) {
                self.plan = Some(plan);
                return false;
            }
            self.next_step = 0;
        }
        // No plan. A search that failed on exactly this input would fail the same way.
        let failed_here = self
            .failed_for
            .as_ref()
            .is_some_and(|(failed_state, failed_goal)| failed_state == state && *failed_goal == self.goal);
        !failed_here
    }

    /// Starts a search for `state` on the pool.
    fn start(&mut self, state: &WorldState, tick: u64, latency: u32) {
        let planner = Arc::clone(&self.planner);
        let (start, goal, options) = (*state, self.goal.clone(), self.options);
        let request_goal = goal.clone();
        let task =
            AsyncComputePool::get().spawn(async move { planner.plan(&start, &request_goal, &options) });
        self.pending = Some(PendingPlan {
            task,
            ready_tick: tick.saturating_add(u64::from(latency.max(1))),
            start,
            goal,
            options,
        });
    }

    /// Takes the running search's result once its latency is up. `None` when it is not time yet;
    /// otherwise whether the search had to run here because the pool had not finished it.
    fn harvest(&mut self, tick: u64) -> Option<bool> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| tick >= pending.ready_tick)
        {
            return None;
        }
        let pending = self.pending.take()?;
        let inline = !pending.task.is_finished();
        let result = if inline {
            // Dropping the task cancels it, or lets it finish unseen if it is already running.
            drop(pending.task);
            self.planner.plan(&pending.start, &pending.goal, &pending.options)
        } else {
            block_on(pending.task)
        };
        // What the result is about: `look` compares the state now against the state this search saw.
        self.last_state = Some(pending.start);
        match result {
            Ok(plan) => {
                self.plan = Some(plan);
                self.next_step = 0;
                self.step_base = pending.start;
                self.failed_for = None;
                self.last_error = None;
            }
            Err(error) => {
                self.plan = None;
                self.next_step = 0;
                self.failed_for = Some((pending.start, pending.goal));
                self.last_error = Some(error);
            }
        }
        Some(inline)
    }

    /// The state to show.
    fn state_name(&self) -> GoapState {
        if self.pending.is_some() {
            GoapState::Planning
        } else if self.plan.is_some() {
            GoapState::Executing
        } else if self
            .last_state
            .as_ref()
            .is_some_and(|s| self.goal.is_satisfied_by(s))
        {
            GoapState::Satisfied
        } else if self.last_error.is_some() {
            GoapState::Failed
        } else {
            GoapState::Idle
        }
    }
}

/// Writes the agent's view to its status and the current action to its blackboard, touching neither
/// when nothing changed.
fn publish(agent: &GoapAgent, status: &mut Mut<GoapStatus>, blackboard: &mut Mut<Blackboard>) {
    let plan = agent.plan.as_ref();
    let same_steps = plan.map_or(status.steps.is_empty(), |plan| {
        plan.len() == status.steps.len()
            && plan
                .action_names()
                .zip(&status.steps)
                .all(|(a, b)| a == b.as_str())
    });
    let error = match (agent.state_name(), agent.last_error) {
        (GoapState::Failed, Some(error)) => plan_error_name(error),
        _ => "",
    };
    let state = agent.state_name();
    let total_cost = plan.map_or(0.0, Plan::total_cost);
    let nodes = plan.map_or(0, |p| u32::try_from(p.nodes_expanded()).unwrap_or(u32::MAX));
    let next_step = u32::try_from(agent.next_step).unwrap_or(u32::MAX);
    if status.state != state
        || !same_steps
        || status.next_step != next_step
        || status.total_cost != total_cost
        || status.nodes_expanded != nodes
        || status.error != error
    {
        status.state = state;
        if !same_steps {
            status.steps = plan.map_or_else(Vec::new, |p| p.action_names().map(str::to_string).collect());
        }
        status.next_step = next_step;
        status.total_cost = total_cost;
        status.nodes_expanded = nodes;
        status.error = error.to_string();
    }
    if !agent.action_key.is_empty() {
        match agent.current_action() {
            Some(name) => {
                if blackboard.get_string(&agent.action_key) != name {
                    blackboard
                        .values
                        .insert(agent.action_key.clone(), BbValue::String(name.to_string()));
                }
            }
            None => {
                if blackboard.contains(&agent.action_key) {
                    blackboard.remove(&agent.action_key);
                }
            }
        }
    }
}

/// `FixedUpdate`: one step of every agent (see the module docs).
#[tracing::instrument(skip_all, target = "regia_ai", name = "goap_tick")]
fn goap_tick(
    settings: Res<GoapSettings>,
    mut stats: ResMut<GoapStats>,
    mut agents: Query<(Entity, &mut GoapAgent, &mut GoapStatus, &mut Blackboard), Without<LayerParked>>,
    mut parked: Query<&mut GoapAgent, With<LayerParked>>,
    spawn_order: Option<Res<regia_ecs::spawn_order::SpawnOrder<GoapAgent>>>,
    mut order: Local<Vec<Entity>>,
) {
    // A write to a parked agent's blackboard is out of sight of the change check by the time it
    // comes back, so a returning agent always looks.
    for mut agent in &mut parked {
        if !agent.needs_look {
            agent.needs_look = true;
        }
    }
    stats.tick += 1;
    let tick = stats.tick;
    stats.plans_started_last_tick = 0;
    order.clear();
    order.extend(agents.iter().map(|row| row.0));
    crate::agent_order_by(spawn_order.as_deref(), &mut order);
    for &entity in order.iter() {
        let Ok((_, mut agent, mut status, mut blackboard)) = agents.get_mut(entity) else {
            continue;
        };
        let harvested = agent.harvest(tick);
        stats.searches_run_inline += u64::from(harvested == Some(true));
        if harvested.is_none() && !agent.needs_look && !blackboard.is_changed() {
            continue;
        }
        let state = agent.facts.snapshot(&blackboard);
        let wants_plan = agent.look(&state);
        agent.needs_look = false;
        if wants_plan && !agent.is_planning() {
            if stats.plans_started_last_tick < settings.max_plans_per_tick {
                stats.plans_started_last_tick += 1;
                stats.plans_started_total += 1;
                status.replans += u32::from(status.plans_started > 0);
                status.plans_started += 1;
                agent.start(&state, tick, settings.plan_latency_ticks);
            } else {
                // No room this step: look again next step.
                agent.needs_look = true;
            }
        }
        publish(&agent, &mut status, &mut blackboard);
    }
}
