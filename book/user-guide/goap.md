# Goal-oriented action planning (GOAP)

`regia_ai::goap` is a planner for agents that choose *what to do next* by chaining actions:
give it the actions an agent can perform (each with preconditions, effects and a cost), the
world as it is now, and a goal as a partial description of the world as it should be. It
returns a minimum-cost sequence of actions that reaches the goal, or says why it cannot.
The formulation is Jeff Orkin's GOAP ("Three States and a Plan", GDC 2006); see
`docs/provenance.md` for the sources.

GOAP complements behavior trees: a tree decides *that* the agent wants a table, the planner
works out *how* (fetch an axe, chop, craft — or the expensive stick-collecting fallback when
no axe exists), and tree leaf tasks execute the plan steps by action name.

## World state and facts

A `WorldState` is a fixed-size snapshot: up to 128 boolean facts (`MAX_BOOL_FACTS`, a
two-word bitset) plus up to 16 small integer facts (`MAX_INT_FACTS`, counts, resource
amounts). Facts are addressed by index; the meaning of each index is your game's convention.
A consistent bijection of the used boolean and integer indices preserves a planning problem:
relabel the start state, partial goal, and every action precondition and effect together. A
unique minimum-cost plan then keeps the same action-name sequence and cost, and its final state
maps back through the inverse bijection. Equal-cost alternatives need only preserve plan cost
and validity; tie order is deterministic for a fixed action declaration order, not invariant to
fact indices. Successful cache entries remain specific to the start state, goal and options.
A `FactTable` maps blackboard key names to fact indices once, then snapshots a `Blackboard`
into a `WorldState` to plan from (and can write a simulated state back):

```rust
use regia_ai::Blackboard;
use regia_ai::goap::{FactTable, WorldState};

fn snapshot_facts(blackboard: &Blackboard) -> WorldState {
    let mut facts = FactTable::new();
    let _has_axe = facts.add_bool("has_axe").expect("under the 128-fact limit");
    let _wood = facts.add_int("wood").expect("under the 16-fact limit");
    // Later, per planning request:
    facts.snapshot(blackboard)
}
```

Out-of-range fact indices never panic anywhere in the module: reads return the default
(`false` / `0`). Boolean builders and `Goal` ignore an index at or past the table (128 boolean
facts, 16 integer facts). An action's integer precondition or effect at or past 16 is stored, and
`Planner::plan` and `Planner::try_new` return `PlanError::InvalidAction` for that action.

## Actions, goals, planning

```rust
use regia_ai::goap::{Action, Goal, Plan, PlanError, PlanOptions, Planner, WorldState};

fn plan_for_planks() -> Result<Plan, PlanError> {
    // Fact indices: 0 = has_axe, 1 = has_wood, 2 = has_planks.
    let planner = Planner::new(vec![
        Action::new("pick_up_axe", 1.0).sets(0, true),
        Action::new("chop_tree", 2.0).requires(0, true).sets(1, true),
        Action::new("saw_planks", 1.0).requires(1, true).sets(2, true),
    ]);
    let start = WorldState::new();
    let goal = Goal::new().require(2, true);
    let plan = planner.plan(&start, &goal, &PlanOptions::default())?;
    for step in plan.steps() {
        // step.name, step.action_index, step.cost
        let _ = step;
    }
    Ok(plan)
}
```

- An `Action` has boolean preconditions/effects (`requires` / `sets`), optional integer
  preconditions (`requires_int` with an `IntCondition`) and effects (`sets_int` /
  `adds_int`, saturating), and a static cost — or a dynamic cost callback
  (`with_dynamic_cost`) for state-dependent pricing (a shortcut that is only cheap while a
  gate is open). The callback must be a pure, deterministic function of the state returning
  a finite, non-negative cost.
- A `Goal` is partial: only the facts it mentions must match. Per-fact weights
  (`with_weight`, `require_int_weighted`) shape the search heuristic only, never success.
- `Planner::plan` runs a forward A* search with an admissible heuristic (the weighted count
  of unsatisfied goal facts, scaled so it never overestimates), so the returned plan has
  minimum total cost. Ties break deterministically in action-declaration order: the same
  request always returns the same plan.
- Errors are `PlanError::NoPlan` (goal unreachable), `PlanError::LimitReached` (the
  `PlanOptions` node or depth budget ran out first — a plan may still exist) and
  `PlanError::InvalidAction` (empty action name, negative/non-finite cost including from a
  dynamic callback, integer fact out of range). `Planner::try_new` rejects a malformed
  action set at build time. A start state that already satisfies the goal is an empty plan,
  not an error.

`PlanOptions` sets the node budget (`max_nodes`, default 10 000), the plan length cap
(`max_depth`, default 64), the heuristic weight (`1.0` optimal; `0.0` is Dijkstra; above
`1.0` is faster weighted A* with bounded suboptimality) and whether to use the planner's
plan cache. Successful plans are cached by (state, goal, options); `Plan::cache_hit` tells
you a plan came from the cache, and adding an action clears it.

## Executing and replanning

Plans go stale: the axe breaks, another agent takes the wood. Before running each step (or
when the world changes), re-validate the remaining suffix against the current state:

```rust,wrap
# use regia_ai::goap::{Action, Goal, PlanOptions, Planner, WorldState};
# let planner = Planner::new(vec![
#     Action::new("pick_up_axe", 1.0).sets(0, true),
#     Action::new("chop_tree", 2.0).requires(0, true).sets(1, true),
#     Action::new("saw_planks", 1.0).requires(1, true).sets(2, true),
# ]);
# let current_state = WorldState::new();
# let goal = Goal::new().require(2, true);
# let options = PlanOptions::default();
# let plan = planner.plan(&current_state, &goal, &options)?;
# let next_step = 0;
let verdict = plan.validate_remaining(&current_state, next_step);
if !verdict.is_valid() {
    // verdict.first_invalid_step names the step whose preconditions stopped holding.
    let plan = planner.replan(&plan, &current_state, next_step, &goal, &options)?;
}
```

`Plan::validate(&state)` is the whole-plan form. `Planner::replan` keeps the still-valid
suffix unchanged (so an agent does not jitter between equal routes) and only searches
afresh when a precondition actually stopped holding, the suffix no longer reaches the
goal, or the goal changed. `Plan::simulate` gives the state a suffix would reach, which is
handy for debugging and editor previews.

## Limits, ties and cost

- **Depth limit.** `max_depth` is exact: if any plan of at most `max_depth` steps exists, the
  cheapest such plan is returned (within the node budget), even when a cheaper but longer
  plan exists. `LimitReached` from the depth limit means the search ran out of room, not
  that no plan exists.
- **Ties.** Between plans of equal cost, a path with fewer steps to the same state is kept;
  among equal-cost, equal-length plans the earlier-declared actions win. The result never
  depends on hash order or timing.
- **Threads.** `Planner`, `Plan`, `Goal` and `FactTable` are `Send + Sync`, so a planner can
  live in a component or resource; the plan cache is behind a lock.
- **Search effort.** The heuristic counts unsatisfied goal facts, which guides the search
  well when each action changes a few goal facts and poorly in dense domains where many
  actions touch many facts. On a seeded 128-fact, 60-action domain, goals reachable in 4
  actions expand a median of 540 nodes (worst of 20 seeds about 13 000), goals reachable
  in 6 actions a median of 6 700 (worst about 196 000). Keep action sets small per agent,
  keep `max_nodes` as a budget, and treat `LimitReached` as "replan later or fall back".

## On entities: `GoapAgent`

`GoapPlugin` (opt-in, ADR-2565) runs a planner per entity. Build the `Planner` once and share it between agents of a kind; its
plan cache is shared with it. The agent needs a `Blackboard`, which is where its facts come from:

```rust
use std::sync::Arc;

use regia_ai::goap::{Action, FactTable, Goal, Planner};
use regia_ai::{Blackboard, GoapAgent};

fn guard_agent() -> (GoapAgent, Blackboard) {
    let mut facts = FactTable::new();
    let has_wood = facts.add_bool("has_wood").unwrap_or_default();
    let has_table = facts.add_bool("has_table").unwrap_or_default();
    let planner = Arc::new(Planner::new(vec![
        Action::new("gather", 1.0).sets(has_wood, true),
        Action::new("craft", 1.0).requires(has_wood, true).sets(has_table, true),
    ]));
    let goal = Goal::new().require(has_table, true);
    (GoapAgent::new(planner, facts, goal), Blackboard::new())
}
```

Add `GoapPlugin` and spawn both on the entity. Each fixed step in `GoapStep`, before `AiStep`, an agent whose blackboard changed:

1. takes a finished search's result, exactly `GoapSettings::plan_latency_ticks` steps after it started;
2. reads its facts from the blackboard;
3. advances past each step whose effects now hold;
4. searches again when a remaining precondition no longer holds or the goal moved.

The planner does not run actions: game code does. Read the action name from the blackboard key `goap_action`
(`GoapAgent::action_key`), for example from a behavior-tree leaf, and write the effect's fact when the work is done. A valid plan
is kept when a cheaper one appears unless `replan_on_any_change` is set. `GoapAgent::set_goal` changes the goal.

**Budget.** A search stops after `options.max_nodes` expanded nodes (default 2 000) with `limit_reached`, and
`GoapSettings::max_plans_per_tick` limits how many start per step. A search that failed is not repeated until the facts or the
goal change, since it would fail the same way; call `GoapAgent::retry` after raising the budget.

**Determinism.** A search runs on the async compute pool, but its result is taken at a fixed step, never earlier. If the pool has
not finished it by then, the same search runs on the fixed step's thread, bounded by the budget, and gives the same plan. Two runs
replan on the same steps and get the same plans on any machine. Raise `plan_latency_ticks` if `GoapStats::searches_run_inline`
climbs.

`GoapStatus` is the reflected view (`state`, `steps`, `next_step`, `total_cost`, `replans`, `error`). The agent method
`ai.goap_plan` shows the whole plan (it answers `ai_goap_unavailable` without `GoapPlugin`): see
[AI perception](../agents/ai-perception.md).
