// SPDX-License-Identifier: MPL-2.0
//! Metamorphic checks for GOAP's dependence on fact indices.

use regia_ai::goap::{
    Action, Goal, IntCmp, IntCondition, IntEffectKind, MAX_BOOL_FACTS, MAX_INT_FACTS, Plan, PlanOptions,
    Planner, WorldState,
};

const BOOL_MAP: [usize; 4] = [127, 64, 63, 0];
const INT_MAP: [usize; 2] = [15, 0];

fn source_actions() -> Vec<Action> {
    vec![
        Action::new("prepare", 1.0)
            .requires(0, true)
            .requires_int(IntCondition::new(0, IntCmp::Ge, 2))
            .sets(1, true)
            .adds_int(1, 1),
        Action::new("finish", 1.0)
            .requires(1, true)
            .sets(2, true)
            .adds_int(1, 2),
        Action::new("shortcut", 9.0)
            .requires(0, true)
            .sets(2, true)
            .sets_int(1, 3),
        Action::new("wrong_gate", 0.1).requires(3, true).sets(2, true),
    ]
}

fn source_state() -> WorldState {
    WorldState::new()
        .with(0, true)
        .with(3, false)
        .with_int(0, 2)
        .with_int(1, 0)
}

fn source_goal() -> Goal {
    Goal::new()
        .require(2, true)
        .require_int(IntCondition::new(1, IntCmp::Ge, 3))
}

fn relabel_state(state: &WorldState) -> WorldState {
    let mut mapped = WorldState::new();
    for (source, &target) in BOOL_MAP.iter().enumerate() {
        mapped.set(target, state.get(source));
    }
    for (source, &target) in INT_MAP.iter().enumerate() {
        mapped.set_int(target, state.get_int(source));
    }
    mapped
}

fn inverse_state(state: &WorldState) -> WorldState {
    let mut mapped = WorldState::new();
    for (source, &target) in BOOL_MAP.iter().enumerate() {
        mapped.set(source, state.get(target));
    }
    for (source, &target) in INT_MAP.iter().enumerate() {
        mapped.set_int(source, state.get_int(target));
    }
    mapped
}

fn relabel_goal(goal: &Goal) -> Goal {
    let mut mapped = Goal::new();
    for (source, &target) in BOOL_MAP.iter().enumerate() {
        if let Some(value) = goal.bool_conditions().get(source) {
            mapped = mapped.require(target, value);
        }
    }
    for cond in goal.int_conditions() {
        let target = INT_MAP[usize::from(cond.fact)];
        mapped = mapped.require_int(IntCondition::new(target as u8, cond.cmp, cond.value));
    }
    mapped
}

fn relabel_action(action: &Action) -> Action {
    let mut mapped = Action::new(action.name.clone(), action.cost);
    for (source, &target) in BOOL_MAP.iter().enumerate() {
        if let Some(value) = action.preconditions.get(source) {
            mapped = mapped.requires(target, value);
        }
        if let Some(value) = action.effects.get(source) {
            mapped = mapped.sets(target, value);
        }
    }
    for cond in &action.int_preconditions {
        mapped = mapped.requires_int(IntCondition::new(
            INT_MAP[usize::from(cond.fact)] as u8,
            cond.cmp,
            cond.value,
        ));
    }
    for effect in &action.int_effects {
        let fact = INT_MAP[usize::from(effect.fact)] as u8;
        mapped = match effect.kind {
            IntEffectKind::Set(value) => mapped.sets_int(fact, value),
            IntEffectKind::Add(delta) => mapped.adds_int(fact, delta),
        };
    }
    mapped
}

fn simulate(plan: &Plan, actions: &[Action], start: WorldState) -> WorldState {
    let mut state = start;
    for step in plan.steps() {
        let action = &actions[step.action_index];
        assert!(action.is_applicable(&state), "{} must be applicable", step.name);
        state = action.apply(&state);
    }
    state
}

fn names(plan: &Plan) -> Vec<&str> {
    plan.action_names().collect()
}

#[test]
fn bijective_boolean_and_integer_relabeling_preserves_unique_optimum() {
    assert_eq!(MAX_BOOL_FACTS, 128);
    assert_eq!(MAX_INT_FACTS, 16);
    let actions = source_actions();
    let mapped_actions: Vec<_> = actions.iter().map(relabel_action).collect();
    let planner = Planner::new(actions.clone());
    let mapped_planner = Planner::new(mapped_actions.clone());
    let start = source_state();
    let goal = source_goal();
    let mapped_start = relabel_state(&start);
    let mapped_goal = relabel_goal(&goal);
    for heuristic_weight in [0.0, 1.0] {
        let options = PlanOptions {
            heuristic_weight,
            use_cache: false,
            ..PlanOptions::default()
        };
        let original = planner
            .plan(&start, &goal, &options)
            .expect("bounded source plan");
        let relabeled = mapped_planner
            .plan(&mapped_start, &mapped_goal, &options)
            .expect("bounded relabeled plan");

        assert_eq!(names(&original), ["prepare", "finish"]);
        assert_eq!(names(&relabeled), names(&original));
        assert_eq!(relabeled.total_cost(), original.total_cost());
        let final_state = simulate(&original, &actions, start);
        let mapped_final = simulate(&relabeled, &mapped_actions, mapped_start);
        let reference_final = WorldState::new()
            .with(0, true)
            .with(1, true)
            .with(2, true)
            .with_int(0, 2)
            .with_int(1, 3);
        assert!(goal.is_satisfied_by(&final_state));
        assert!(mapped_goal.is_satisfied_by(&mapped_final));
        assert_eq!(final_state, reference_final);
        assert_eq!(inverse_state(&mapped_final), final_state);
    }
}

#[test]
fn cache_separates_goals_and_external_start_changes() {
    let planner = Planner::new(vec![
        Action::new("open_gate", 1.0).sets(0, true),
        Action::new("raise_flag", 2.0).sets(1, true),
        Action::new("close_gate", 0.25).sets(0, false),
        Action::new("raise_flag_with_gate_closed", 0.5)
            .requires(0, false)
            .sets(1, true),
    ]);
    let options = PlanOptions::default();
    let start = WorldState::new();
    let gate_goal = Goal::new().require(0, true);
    let flag_goal = Goal::new().require(1, true);

    let gate_cold = planner.plan(&start, &gate_goal, &options).expect("gate plan");
    assert!(!gate_cold.cache_hit());
    let gate_warm = planner
        .plan(&start, &gate_goal, &options)
        .expect("cached gate plan");
    assert!(gate_warm.cache_hit());
    assert_eq!(names(&gate_warm), names(&gate_cold));

    let flag_cold = planner.plan(&start, &flag_goal, &options).expect("flag plan");
    assert!(!flag_cold.cache_hit(), "another goal must miss the cache");
    assert_eq!(names(&flag_cold), ["raise_flag_with_gate_closed"]);
    let flag_warm = planner
        .plan(&start, &flag_goal, &options)
        .expect("cached flag plan");
    assert!(flag_warm.cache_hit());

    let changed_start = start.with(0, true);
    let changed = planner
        .replan(&flag_cold, &changed_start, 0, &flag_goal, &options)
        .expect("changed-start plan");
    assert!(
        !changed.cache_hit(),
        "an external fact change changes the cache key"
    );
    assert_eq!(names(&changed), ["close_gate", "raise_flag_with_gate_closed"]);
    assert!(simulate(&changed, &planner_actions(), changed_start).satisfies(&flag_goal));
}

fn planner_actions() -> Vec<Action> {
    vec![
        Action::new("open_gate", 1.0).sets(0, true),
        Action::new("raise_flag", 2.0).sets(1, true),
        Action::new("close_gate", 0.25).sets(0, false),
        Action::new("raise_flag_with_gate_closed", 0.5)
            .requires(0, false)
            .sets(1, true),
    ]
}

#[test]
fn tied_relabelings_require_validity_and_cost_not_order_identity() {
    let actions = vec![
        Action::new("route_a", 1.0).sets(0, true),
        Action::new("route_b", 1.0).sets(0, true),
    ];
    let mapped: Vec<_> = actions.iter().map(relabel_action).collect();
    let start = WorldState::new();
    let goal = Goal::new().require(0, true);
    let options = PlanOptions {
        heuristic_weight: 1.0,
        use_cache: false,
        ..PlanOptions::default()
    };
    let original = Planner::new(actions.clone())
        .plan(&start, &goal, &options)
        .expect("original tied plan");
    let relabeled = Planner::new(mapped.clone())
        .plan(&relabel_state(&start), &relabel_goal(&goal), &options)
        .expect("relabelled tied plan");
    assert_eq!(original.total_cost(), relabeled.total_cost());
    assert!(original.validate(&start));
    assert!(relabeled.validate(&relabel_state(&start)));
    let original_final = simulate(&original, &actions, start);
    let mapped_final = simulate(&relabeled, &mapped, relabel_state(&start));
    assert!(goal.is_satisfied_by(&original_final));
    assert!(relabel_goal(&goal).is_satisfied_by(&mapped_final));
    assert_eq!(inverse_state(&mapped_final), original_final);
}
