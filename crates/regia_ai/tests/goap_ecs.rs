// SPDX-License-Identifier: MPL-2.0
//! GOAP on entities (ADR-2565): fixed steps only, no wall clock.

use std::sync::{Arc, Mutex};

use regia_ai::Blackboard;
use regia_ai::goap::{Action, FactTable, Goal, IntCmp, IntCondition, PlanOptions, Planner};
use regia_ai::goap_ecs::{GoapAgent, GoapPlugin, GoapSettings, GoapState, GoapStats, GoapStatus};
use regia_app::prelude::*;
use regia_scene::layers::LayerParked;

const HAS_AXE: usize = 0;
const HAS_WOOD: usize = 1;
const HAS_TABLE: usize = 2;
const AXE_AVAILABLE: usize = 3;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, GoapPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

fn step(app: &mut App, n: u64) {
    let goal = app.world().resource::<GoapStats>().tick + n;
    for _ in 0..(n + 2) * 64 {
        if app.world().resource::<GoapStats>().tick >= goal {
            return;
        }
        app.update();
    }
    panic!("goap did not reach tick {goal}");
}

fn facts() -> FactTable {
    let mut table = FactTable::new();
    for name in ["has_axe", "has_wood", "has_table", "axe_available"] {
        table.add_bool(name).expect("under the limit");
    }
    table
}

/// Chop with an axe if one lies around, else the costly way by hand.
fn woodcutter() -> Arc<Planner> {
    Arc::new(Planner::new(vec![
        Action::new("pick_up_axe", 1.0)
            .requires(AXE_AVAILABLE, true)
            .sets(HAS_AXE, true),
        Action::new("chop", 1.0)
            .requires(HAS_AXE, true)
            .sets(HAS_WOOD, true),
        Action::new("gather_sticks", 8.0).sets(HAS_WOOD, true),
        Action::new("craft", 1.0)
            .requires(HAS_WOOD, true)
            .sets(HAS_TABLE, true),
    ]))
}

fn spawn(app: &mut App, axe: bool) -> Entity {
    let mut board = Blackboard::new();
    board.insert("axe_available", axe);
    app.world_mut()
        .spawn((
            GoapAgent::new(woodcutter(), facts(), Goal::new().require(HAS_TABLE, true)),
            board,
        ))
        .id()
}

fn status(app: &App, agent: Entity) -> GoapStatus {
    app.world().get::<GoapStatus>(agent).expect("status").clone()
}

fn set_fact(app: &mut App, agent: Entity, key: &str, value: bool) {
    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("blackboard")
        .insert(key, value);
}

fn action(app: &App, agent: Entity) -> String {
    app.world()
        .get::<Blackboard>(agent)
        .expect("blackboard")
        .get_string("goap_action")
        .to_string()
}

/// The search runs across a fixed-step boundary: planning on one step, the plan on the next.
#[test]
fn an_agent_plans_on_the_pool_and_the_plan_lands_a_fixed_latency_later() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    step(&mut app, 1);
    assert_eq!(status(&app, agent).state, GoapState::Planning);
    assert_eq!(action(&app, agent), "");
    step(&mut app, 1);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Executing);
    assert_eq!(view.steps, ["pick_up_axe", "chop", "craft"]);
    assert_eq!(view.next_step, 0);
    assert_eq!(view.total_cost, 3.0);
    assert_eq!(action(&app, agent), "pick_up_axe");
    assert!(view.nodes_expanded > 0);
}

/// The search does run on the pool: a cost callback sees the pool's thread.
#[test]
fn the_search_runs_on_a_pool_thread() {
    let names: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&names);
    let (observed_tx, observed_rx) = std::sync::mpsc::channel();
    let planner = Arc::new(Planner::new(vec![
        Action::new("work", 1.0)
            .sets(0, true)
            .with_dynamic_cost(move |_| {
                let name = std::thread::current().name().unwrap_or("").to_string();
                seen.lock().expect("lock").push(name);
                observed_tx.send(()).expect("the test observes the pool task");
                1.0
            }),
    ]));
    let mut facts = FactTable::new();
    facts.add_bool("done").expect("fact");
    let mut app = app();
    app.world_mut().spawn((
        GoapAgent::new(planner, facts, Goal::new().require(0, true)),
        Blackboard::new(),
    ));
    // Observe the submitted task before advancing to its harvest tick. At that tick a busy
    // pool deliberately triggers the bounded inline fallback, which this test does not exercise.
    step(&mut app, 1);
    observed_rx.recv().expect("the submitted search runs on the pool");
    let names = names.lock().expect("lock");
    assert!(!names.is_empty());
    let here = std::thread::current().name().unwrap_or("").to_string();
    assert!(
        names
            .iter()
            .all(|name| name.contains("regia-async") && *name != here),
        "{names:?}"
    );
}

/// The agent follows the plan as the game makes the effects true, and stops at the goal.
#[test]
fn the_plan_advances_as_effects_become_true_and_ends_at_the_goal() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    step(&mut app, 2);
    set_fact(&mut app, agent, "has_axe", true);
    step(&mut app, 1);
    assert_eq!(action(&app, agent), "chop");
    assert_eq!(status(&app, agent).next_step, 1);
    set_fact(&mut app, agent, "has_wood", true);
    step(&mut app, 1);
    assert_eq!(action(&app, agent), "craft");
    set_fact(&mut app, agent, "has_table", true);
    step(&mut app, 1);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Satisfied);
    assert_eq!(view.steps, Vec::<String>::new());
    assert_eq!(action(&app, agent), "", "no current action at the goal");
    assert_eq!(
        view.plans_started, 1,
        "following the plan needed no second search"
    );
}

/// Acceptance: a GOAP agent replans when a precondition changes.
#[test]
fn an_agent_replans_when_a_precondition_changes() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["pick_up_axe", "chop", "craft"]);
    assert_eq!(status(&app, agent).replans, 0);

    // Someone takes the axe: pick_up_axe's precondition no longer holds.
    set_fact(&mut app, agent, "axe_available", false);
    step(&mut app, 1);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Planning, "the old plan is gone");
    assert_eq!(view.replans, 1);
    assert_eq!(action(&app, agent), "", "no stale action while planning");
    step(&mut app, 1);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Executing);
    assert_eq!(view.steps, ["gather_sticks", "craft"], "the costly way");
    assert_eq!(view.total_cost, 9.0);
    assert_eq!(action(&app, agent), "gather_sticks");
}

/// A change that leaves the plan valid costs no search, unless the agent asks for one.
#[test]
fn a_change_that_keeps_the_plan_valid_does_not_replan_unless_asked() {
    let mut app = app();
    let agent = spawn(&mut app, false);
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["gather_sticks", "craft"]);
    // An axe appears: a cheaper plan exists, but the current one still works.
    set_fact(&mut app, agent, "axe_available", true);
    step(&mut app, 3);
    let view = status(&app, agent);
    assert_eq!(view.steps, ["gather_sticks", "craft"]);
    assert_eq!(view.replans, 0);

    let mut app = self::app();
    let agent = spawn(&mut app, false);
    app.world_mut()
        .get_mut::<GoapAgent>(agent)
        .expect("agent")
        .replan_on_any_change = true;
    step(&mut app, 2);
    set_fact(&mut app, agent, "axe_available", true);
    step(&mut app, 2);
    let view = status(&app, agent);
    assert_eq!(view.steps, ["pick_up_axe", "chop", "craft"], "the cheaper way");
    assert_eq!(view.replans, 1);
}

/// A new goal replaces the plan.
#[test]
fn a_new_goal_replans_without_a_blackboard_change() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    step(&mut app, 2);
    app.world_mut()
        .get_mut::<GoapAgent>(agent)
        .expect("agent")
        .set_goal(Goal::new().require(HAS_WOOD, true));
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["pick_up_axe", "chop"]);
}

/// The expansion budget bounds a search; the failure names itself and is not repeated every step;
/// raising the budget and asking again works.
#[test]
fn the_expansion_budget_stops_a_search_and_a_failure_is_not_repeated() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    app.world_mut()
        .get_mut::<GoapAgent>(agent)
        .expect("agent")
        .options = PlanOptions {
        max_nodes: 1,
        ..PlanOptions::default()
    };
    step(&mut app, 2);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Failed);
    assert_eq!(view.error, "limit_reached");
    step(&mut app, 5);
    assert_eq!(
        status(&app, agent).plans_started,
        1,
        "the same input fails the same way"
    );

    {
        let mut goap = app.world_mut().get_mut::<GoapAgent>(agent).expect("agent");
        goap.options.max_nodes = 100;
        goap.retry();
    }
    step(&mut app, 2);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Executing);
    assert_eq!(view.error, "");
    assert_eq!(view.plans_started, 2);
}

/// No plan exists: the error says so, and a change to the facts tries again.
#[test]
fn an_unreachable_goal_reports_no_plan_and_a_fact_change_tries_again() {
    let planner = Arc::new(Planner::new(vec![
        Action::new("finish", 1.0).requires(1, true).sets(0, true),
    ]));
    let mut facts = FactTable::new();
    facts.add_bool("done").expect("fact");
    facts.add_bool("ready").expect("fact");
    let mut app = app();
    let agent = app
        .world_mut()
        .spawn((
            GoapAgent::new(planner, facts, Goal::new().require(0, true)),
            Blackboard::new(),
        ))
        .id();
    step(&mut app, 2);
    assert_eq!(status(&app, agent).error, "no_plan");
    assert_eq!(status(&app, agent).state, GoapState::Failed);
    set_fact(&mut app, agent, "ready", true);
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["finish"]);
    assert_eq!(status(&app, agent).error, "");
}

/// A malformed action set is a clean failure.
#[test]
fn a_malformed_planner_reports_invalid_action() {
    let planner = Arc::new(Planner::new(vec![Action::new("", -1.0).sets(0, true)]));
    let mut facts = FactTable::new();
    facts.add_bool("done").expect("fact");
    let mut app = app();
    let agent = app
        .world_mut()
        .spawn((
            GoapAgent::new(planner, facts, Goal::new().require(0, true)),
            Blackboard::new(),
        ))
        .id();
    step(&mut app, 2);
    assert_eq!(status(&app, agent).error, "invalid_action");
}

/// A goal that already holds needs no search at all.
#[test]
fn a_goal_that_already_holds_starts_no_search() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    set_fact(&mut app, agent, "has_table", true);
    step(&mut app, 3);
    let view = status(&app, agent);
    assert_eq!(view.state, GoapState::Satisfied);
    assert_eq!(view.plans_started, 0);
    assert_eq!(view.error, "");
}

/// Facts changing while a search runs: the plan that lands is checked against the state of that step.
#[test]
fn a_plan_that_went_stale_in_flight_is_replaced() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    step(&mut app, 1);
    assert_eq!(status(&app, agent).state, GoapState::Planning);
    set_fact(&mut app, agent, "axe_available", false);
    step(&mut app, 1);
    assert_eq!(
        status(&app, agent).state,
        GoapState::Planning,
        "landed stale, searching again"
    );
    assert_eq!(status(&app, agent).plans_started, 2);
    step(&mut app, 1);
    assert_eq!(status(&app, agent).steps, ["gather_sticks", "craft"]);
}

/// The budget of searches per step holds, in entity order, and the rest are served next step.
#[test]
fn searches_per_step_are_capped_and_the_rest_wait_their_turn() {
    let mut app = app();
    app.world_mut().resource_mut::<GoapSettings>().max_plans_per_tick = 2;
    let agents: Vec<Entity> = (0..5).map(|_| spawn(&mut app, true)).collect();
    step(&mut app, 1);
    assert_eq!(app.world().resource::<GoapStats>().plans_started_last_tick, 2);
    assert_eq!(status(&app, agents[0]).state, GoapState::Planning);
    assert_eq!(status(&app, agents[1]).state, GoapState::Planning);
    assert_eq!(status(&app, agents[2]).state, GoapState::Idle);
    step(&mut app, 1);
    assert_eq!(app.world().resource::<GoapStats>().plans_started_last_tick, 2);
    step(&mut app, 2);
    assert_eq!(app.world().resource::<GoapStats>().plans_started_total, 5);
    assert_eq!(status(&app, agents[4]).state, GoapState::Executing);
}

/// A longer latency delays the landing by exactly that many steps.
#[test]
fn the_latency_is_exact() {
    let mut app = app();
    app.world_mut().resource_mut::<GoapSettings>().plan_latency_ticks = 4;
    let agent = spawn(&mut app, true);
    for _ in 0..4 {
        step(&mut app, 1);
        assert_eq!(status(&app, agent).state, GoapState::Planning);
    }
    step(&mut app, 1);
    assert_eq!(status(&app, agent).state, GoapState::Executing);
}

#[test]
fn a_parked_agent_does_not_plan() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    app.world_mut().entity_mut(agent).insert(LayerParked {
        restore: regia_scene::prelude::Visibility::Inherited,
        inherited: false,
    });
    step(&mut app, 3);
    assert_eq!(status(&app, agent).state, GoapState::Idle);
    assert_eq!(app.world().resource::<GoapStats>().plans_started_total, 0);
}

/// A despawned agent with a search in flight drops the task; nothing else notices.
#[test]
fn despawning_during_a_search_is_clean() {
    let mut app = app();
    let agent = spawn(&mut app, true);
    let other = spawn(&mut app, true);
    step(&mut app, 1);
    app.world_mut().despawn(agent);
    step(&mut app, 2);
    assert_eq!(status(&app, other).state, GoapState::Executing);
}

/// An `Add` effect is judged from the value when the step started, not from a fixed number.
#[test]
fn an_add_effect_counts_from_the_value_at_the_start_of_the_step() {
    let planner = Arc::new(Planner::new(vec![Action::new("gather", 1.0).adds_int(0, 2)]));
    let mut facts = FactTable::new();
    facts.add_int("wood").expect("fact");
    let mut board = Blackboard::new();
    board.insert("wood", 5i64);
    let mut app = app();
    let agent = app
        .world_mut()
        .spawn((
            GoapAgent::new(
                planner,
                facts,
                Goal::new().require_int(IntCondition::new(0, IntCmp::Ge, 9)),
            ),
            board,
        ))
        .id();
    let set_wood = |app: &mut App, wood: i64| {
        app.world_mut()
            .get_mut::<Blackboard>(agent)
            .expect("board")
            .insert("wood", wood);
    };
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["gather", "gather"]);
    set_wood(&mut app, 6);
    step(&mut app, 1);
    assert_eq!(status(&app, agent).next_step, 0, "one wood of the two");
    set_wood(&mut app, 7);
    step(&mut app, 1);
    assert_eq!(status(&app, agent).next_step, 1, "the first step added its two");
    set_wood(&mut app, 9);
    step(&mut app, 1);
    assert_eq!(status(&app, agent).state, GoapState::Satisfied);
    assert_eq!(status(&app, agent).plans_started, 1);
}

/// Acceptance (determinism): two runs make the same plans on the same steps.
#[test]
fn two_runs_plan_the_same() {
    fn run() -> Vec<Vec<GoapStatus>> {
        let mut app = app();
        app.world_mut().resource_mut::<GoapSettings>().max_plans_per_tick = 3;
        let agents: Vec<Entity> = (0..8).map(|i| spawn(&mut app, i % 2 == 0)).collect();
        let mut trace = Vec::new();
        for tick in 0..60usize {
            if tick % 5 == 0 {
                // Every third change gives an axe; the others take it away from whoever planned on it.
                let turn = tick / 5;
                set_fact(
                    &mut app,
                    agents[turn % agents.len()],
                    "axe_available",
                    turn % 3 == 0,
                );
            }
            if tick % 7 == 0 {
                set_fact(
                    &mut app,
                    agents[(tick + 3) % agents.len()],
                    "has_wood",
                    tick % 14 == 0,
                );
            }
            step(&mut app, 1);
            trace.push(agents.iter().map(|&a| status(&app, a)).collect());
        }
        trace
    }
    let (a, b) = (run(), run());
    assert_eq!(a, b);
    let replans: u32 = a.last().expect("steps").iter().map(|s| s.replans).sum();
    assert!(replans > 0, "the run replanned");
}

/// A count that goes past what the step adds still finishes the step; exact equality would leave the
/// plan on this step forever.
#[test]
fn an_add_effect_that_overshoots_still_finishes_the_step() {
    let planner = Arc::new(Planner::new(vec![
        Action::new("gather", 1.0).adds_int(0, 2),
        Action::new("craft", 1.0)
            .requires_int(IntCondition::new(0, IntCmp::Ge, 2))
            .sets(0, true),
    ]));
    let mut facts = FactTable::new();
    facts.add_bool("crafted").expect("fact");
    facts.add_int("wood").expect("fact");
    let mut app = app();
    let agent = app
        .world_mut()
        .spawn((
            GoapAgent::new(planner, facts, Goal::new().require(0, true)),
            Blackboard::new(),
        ))
        .id();
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["gather", "craft"]);
    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("board")
        .insert("wood", 3i64);
    step(&mut app, 1);
    assert_eq!(action(&app, agent), "craft", "3 wood is past the 2 the step adds");
    assert_eq!(status(&app, agent).plans_started, 1);
}

/// A write made while the agent was parked counts when it comes back.
#[test]
fn an_agent_coming_back_from_parking_sees_what_changed() {
    let parked = || LayerParked {
        restore: regia_scene::prelude::Visibility::Inherited,
        inherited: false,
    };
    let mut app = app();
    let agent = spawn(&mut app, true);
    step(&mut app, 2);
    assert_eq!(status(&app, agent).steps, ["pick_up_axe", "chop", "craft"]);
    app.world_mut().entity_mut(agent).insert(parked());
    set_fact(&mut app, agent, "axe_available", false);
    step(&mut app, 3);
    assert_eq!(status(&app, agent).replans, 0, "parked");
    app.world_mut().entity_mut(agent).remove::<LayerParked>();
    step(&mut app, 2);
    let view = status(&app, agent);
    assert_eq!(view.replans, 1);
    assert_eq!(view.steps, ["gather_sticks", "craft"]);
}

/// `replan_on_any_change` also sees a change made while a long search ran.
#[test]
fn replan_on_any_change_sees_a_change_during_the_search() {
    let mut app = app();
    app.world_mut().resource_mut::<GoapSettings>().plan_latency_ticks = 3;
    let agent = spawn(&mut app, false);
    app.world_mut()
        .get_mut::<GoapAgent>(agent)
        .expect("agent")
        .replan_on_any_change = true;
    step(&mut app, 1);
    assert_eq!(status(&app, agent).state, GoapState::Planning);
    // An axe appears during the search. The plan that lands is still valid, but it was made without it.
    set_fact(&mut app, agent, "axe_available", true);
    step(&mut app, 3);
    assert_eq!(
        status(&app, agent).plans_started,
        2,
        "the landed plan is about an old state"
    );
    step(&mut app, 3);
    assert_eq!(status(&app, agent).steps, ["pick_up_axe", "chop", "craft"]);
}
