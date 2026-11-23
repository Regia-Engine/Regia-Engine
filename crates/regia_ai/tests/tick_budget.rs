// SPDX-License-Identifier: MPL-2.0
//! The AI tick budget (`AiTickBudget`, ADR-2709): fixed steps and counters only, no wall clock.

use std::sync::{Arc, Mutex};

use regia_ai::blackboard::Blackboard;
use regia_ai::state_tree::{StateTree, StateTreeDef, StateTreeStatus};
use regia_ai::tick_budget::ai_tick_agent_info;
use regia_ai::{
    AiImportance, AiPlugin, AiTickBudget, AiTickStats, AiTickTier, BehaviorTree, BehaviorTreeBuilder,
    BehaviorTreeStatus, BtContext, BtStatus, BtTask, BtTaskRegistry,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;
use regia_scene::layers::LayerParked;

/// The hash of the decision trace below, recorded on the code before the tick budget existed
/// (lane merge f42594078 plus only `entity_id_order` and this test file; a debug build on the test machine). A change to it means trees with no budget no
/// longer behave as they did.
const RECORDED_DECISION_HASH: u64 = 5_117_107_589_150_394_026;

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

/// Tick order and dt of every leaf call, shared with the tasks.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<(i64, u32)>>>);

/// Writes a pseudo-random threat from a counter on the blackboard, and logs its agent id and dt.
struct Decide {
    log: Log,
}

impl BtTask for Decide {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let dt = ctx.dt;
        let Some(mut board) = ctx.blackboard_mut() else {
            return BtStatus::Failure;
        };
        let id = match board.get("id") {
            Some(regia_ai::BbValue::Int(id)) => *id,
            _ => -1,
        };
        let count = match board.get("count") {
            Some(regia_ai::BbValue::Int(count)) => *count,
            _ => 0,
        };
        board.insert("count", count + 1);
        let threat = ((count + id).wrapping_mul(2_654_435_761) >> 7) & 31;
        board.insert("threat", threat as f32);
        board.insert("alert", threat > 15);
        if let Ok(mut log) = self.log.0.lock() {
            log.push((id, dt.to_bits()));
        }
        if threat % 5 == 0 {
            BtStatus::Failure
        } else if threat % 3 == 0 {
            BtStatus::Running
        } else {
            BtStatus::Success
        }
    }
}

const STATE_TREE: &str = r#"(
    format_version: 2,
    root: "Root",
    states: [
        (name: "Root", children: ["Idle", "Alert"]),
        (name: "Idle", tasks: ["test::Decide"], transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Gt, value: 20.0), target: State("Alert")),
        ]),
        (name: "Alert", tasks: ["test::Decide"], transitions: [
            (trigger: Tick, condition: Compare(key: "threat", op: Lt, value: 8.0), target: State("Idle")),
        ]),
    ],
)"#;

fn behavior_tree(registry: &BtTaskRegistry) -> BehaviorTree {
    let mut b = BehaviorTreeBuilder::new();
    let alert = b.condition("alert?", "alert");
    let attack = b.add(regia_ai::DefNode {
        name: "attack".into(),
        kind: regia_ai::DefKind::Leaf,
        task: "test::Decide".into(),
        ..Default::default()
    });
    let engage = b.sequence("engage", vec![alert, attack]);
    let patrol = b.add(regia_ai::DefNode {
        name: "patrol".into(),
        kind: regia_ai::DefKind::Leaf,
        task: "test::Decide".into(),
        ..Default::default()
    });
    let root = b.selector("root", vec![engage, patrol]);
    let def = b.build(root);
    BehaviorTree::compile(&def, registry).expect("the tree compiles")
}

/// 40 agents (behavior tree, state tree or both) for 240 steps, with one despawned and two spawned
/// along the way. Returns a hash over every leaf call and every agent's statuses and blackboard, per
/// step. `prepare` runs once before the first step.
fn decision_hash(prepare: impl FnOnce(&mut App)) -> u64 {
    let log = Log::default();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    {
        let factory_log = log.clone();
        let mut registry = app.world_mut().resource_mut::<BtTaskRegistry>();
        registry.register_factory("test::Decide", move || {
            Box::new(Decide {
                log: factory_log.clone(),
            })
        });
    }
    prepare(&mut app);
    app.update();
    let def = StateTreeDef::from_ron(STATE_TREE).expect("the state tree parses");
    let spawn = |app: &mut App, id: i64| -> Entity {
        let mut board = Blackboard::new();
        board.insert("id", id);
        let entity = app.world_mut().spawn(board).id();
        let (bt, st) = {
            let registry = app.world().resource::<BtTaskRegistry>();
            (
                behavior_tree(registry),
                StateTree::compile(&def, registry).expect("the state tree compiles"),
            )
        };
        if id % 3 != 2 {
            app.world_mut().entity_mut(entity).insert(bt);
        }
        if id % 3 != 0 {
            app.world_mut().entity_mut(entity).insert(st);
        }
        entity
    };
    let mut agents: Vec<Entity> = (0..40).map(|id| spawn(&mut app, id)).collect();
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for step in 0..240u32 {
        match step {
            60 => {
                let gone = agents.remove(7);
                app.world_mut().despawn(gone);
            }
            90 => agents.push(spawn(&mut app, 40)),
            120 => agents.push(spawn(&mut app, 41)),
            _ => {}
        }
        app.world_mut().run_schedule(FixedUpdate);
        if let Ok(mut log) = log.0.lock() {
            for (id, dt) in log.drain(..) {
                fnv(&mut hash, &id.to_le_bytes());
                fnv(&mut hash, &dt.to_le_bytes());
            }
        }
        for &agent in &agents {
            let world = app.world();
            if let Some(status) = world.get::<BehaviorTreeStatus>(agent) {
                fnv(&mut hash, format!("{:?}", status.last_status).as_bytes());
                fnv(&mut hash, format!("{:?}", status.node_status).as_bytes());
            }
            if let Some(status) = world.get::<StateTreeStatus>(agent) {
                fnv(&mut hash, format!("{:?}", status.status).as_bytes());
                fnv(&mut hash, status.active_path.join("/").as_bytes());
                fnv(&mut hash, &status.ticks.to_le_bytes());
            }
            if let Some(board) = world.get::<Blackboard>(agent) {
                for (key, value) in &board.values {
                    fnv(&mut hash, key.as_bytes());
                    fnv(&mut hash, format!("{value:?}").as_bytes());
                }
            }
        }
    }
    hash
}

#[test]
fn no_budget_is_bit_identical_to_the_recorded_decisions() {
    assert_eq!(decision_hash(|_| {}), RECORDED_DECISION_HASH);
}

#[test]
fn a_budget_that_is_off_is_bit_identical_too() {
    let hash = decision_hash(|app| {
        app.world_mut().insert_resource(AiTickBudget {
            enabled: false,
            max_trees_per_step: 1,
            ..AiTickBudget::default()
        });
    });
    assert_eq!(hash, RECORDED_DECISION_HASH);
}

#[test]
fn a_budget_with_room_for_everything_gives_the_same_decisions() {
    // Cadence 1 with no limit: every tree due every step, with exactly the fixed step, in arrival order.
    let on = decision_hash(|app| {
        app.world_mut().insert_resource(AiTickBudget {
            enabled: true,
            max_trees_per_step: 0,
            tiers: vec![AiTickTier::default()],
            focus: None,
        });
    });
    assert_eq!(on, RECORDED_DECISION_HASH);
}

#[test]
fn two_budgeted_runs_hash_equal() {
    let budget = || AiTickBudget {
        enabled: true,
        max_trees_per_step: 7,
        tiers: vec![
            AiTickTier {
                max_distance: 5.0,
                cadence_steps: 2,
                max_trees_per_step: 0,
            },
            AiTickTier {
                max_distance: f32::MAX,
                cadence_steps: 8,
                max_trees_per_step: 3,
            },
        ],
        focus: None,
    };
    let a = decision_hash(|app| app.world_mut().insert_resource(budget()));
    let b = decision_hash(|app| app.world_mut().insert_resource(budget()));
    assert_eq!(a, b);
    assert_ne!(a, RECORDED_DECISION_HASH, "the budget does change who ticks when");
}

/// One tick of one tree: (agent id, dt).
#[derive(Clone, Default)]
struct Ticks(Arc<Mutex<Vec<(i64, f32)>>>);

struct Count {
    ticks: Ticks,
}

impl BtTask for Count {
    fn tick(&mut self, ctx: &mut BtContext<'_>) -> BtStatus {
        let dt = ctx.dt;
        let id = match ctx.blackboard().and_then(|board| board.get("id")) {
            Some(regia_ai::BbValue::Int(id)) => *id,
            _ => -1,
        };
        if let Ok(mut ticks) = self.ticks.0.lock() {
            ticks.push((id, dt));
        }
        BtStatus::Success
    }
}

struct Rig {
    app: App,
    ticks: Ticks,
    focus: Entity,
    /// One fixed step, as the tree systems read it.
    step: f32,
}

fn rig(budget: AiTickBudget) -> Rig {
    let ticks = Ticks::default();
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    {
        let factory = ticks.clone();
        app.world_mut()
            .resource_mut::<BtTaskRegistry>()
            .register_factory("test::Count", move || {
                Box::new(Count {
                    ticks: factory.clone(),
                })
            });
    }
    let focus = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::ZERO))
        .id();
    app.update();
    app.world_mut().insert_resource(AiTickBudget {
        focus: Some(focus),
        ..budget
    });
    let step = app.world().resource::<FixedTime>().step().as_secs_f32();
    Rig {
        app,
        ticks,
        focus,
        step,
    }
}

impl Rig {
    fn agent(&mut self, id: i64, x: f32) -> Entity {
        let tree = {
            let registry = self.app.world().resource::<BtTaskRegistry>();
            BehaviorTree::compile(&regia_ai::BehaviorTreeDef::leaf("l", "test::Count"), registry)
                .expect("compiles")
        };
        let mut board = Blackboard::new();
        board.insert("id", id);
        self.app
            .world_mut()
            .spawn((tree, board, Transform::from_translation(Vec3::new(x, 0.0, 0.0))))
            .id()
    }

    /// Runs `n` fixed steps and returns each step's ticks.
    fn steps(&mut self, n: usize) -> Vec<Vec<(i64, f32)>> {
        (0..n)
            .map(|_| {
                self.app.world_mut().run_schedule(FixedUpdate);
                self.ticks
                    .0
                    .lock()
                    .map(|mut t| t.drain(..).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect()
    }
}

/// The steps each agent ticked on, for ids `0..agents`.
fn ticks_by_agent(steps: &[Vec<(i64, f32)>], agents: usize) -> Vec<Vec<usize>> {
    let mut out = vec![Vec::new(); agents];
    for (step, ticks) in steps.iter().enumerate() {
        for (id, _) in ticks {
            out[*id as usize].push(step);
        }
    }
    out
}

fn tier(max_distance: f32, cadence_steps: u32) -> AiTickTier {
    AiTickTier {
        max_distance,
        cadence_steps,
        max_trees_per_step: 0,
    }
}

// The brief's acceptance: 100 agents on a budget of 10 trees a step.
#[test]
fn ten_trees_a_step_with_100_agents_ticks_everyone_within_cadence() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 10,
        tiers: vec![tier(f32::MAX, 10)],
        focus: None,
    });
    for id in 0..100 {
        rig.agent(id, 1.0);
    }
    let steps = rig.steps(300);
    for (step, ticks) in steps.iter().enumerate() {
        assert!(ticks.len() <= 10, "step {step} ticked {}", ticks.len());
    }
    for (id, ticks) in ticks_by_agent(&steps, 100).iter().enumerate() {
        assert!(ticks[0] < 10, "agent {id} first ticked on step {}", ticks[0]);
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], 10, "agent {id}: {pair:?}");
        }
    }
    // Round robin in the stable order: agents 0-9 first, then 10-19, every cycle.
    let first: Vec<i64> = steps[0].iter().map(|(id, _)| *id).collect();
    assert_eq!(first, (0..10).collect::<Vec<i64>>());
    let second: Vec<i64> = steps[1].iter().map(|(id, _)| *id).collect();
    assert_eq!(second, (10..20).collect::<Vec<i64>>());
    let ten = 10.0 * rig.step;
    assert_eq!(
        steps[10],
        steps[0].iter().map(|(id, _)| (*id, ten)).collect::<Vec<_>>()
    );

    let stats = rig.app.world().resource::<AiTickStats>();
    assert_eq!(stats.steps, 300);
    assert_eq!(stats.trees, 100);
    assert_eq!(stats.ticks_total, 3000);
    assert_eq!(stats.late_ticks_total, 0);
}

#[test]
fn near_agents_tick_often_and_far_ones_at_their_own_cadence_within_the_cap() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 10,
        tiers: vec![tier(20.0, 2), tier(f32::MAX, 18)],
        focus: None,
    });
    // 10 near (10 / 2 = 5 a step) and 90 far (90 / 18 = 5 a step): exactly the budget.
    for id in 0..100 {
        rig.agent(id, if id < 10 { 5.0 } else { 100.0 });
    }
    let steps = rig.steps(360);
    assert!(steps.iter().all(|ticks| ticks.len() <= 10));
    let by_agent = ticks_by_agent(&steps, 100);
    for (id, ticks) in by_agent.iter().enumerate() {
        let cadence = if id < 10 { 2 } else { 18 };
        assert!(ticks[0] < cadence, "agent {id} first ticked on step {}", ticks[0]);
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], cadence, "agent {id}: {pair:?}");
        }
    }
    let stats = rig.app.world().resource::<AiTickStats>();
    assert_eq!(stats.late_ticks_total, 0);
    assert_eq!(stats.tier_trees, vec![10, 90]);
}

#[test]
fn when_demand_exceeds_the_budget_the_cap_holds_and_everyone_still_takes_turns() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 10,
        tiers: vec![tier(f32::MAX, 1)],
        focus: None,
    });
    for id in 0..100 {
        rig.agent(id, 1.0);
    }
    let steps = rig.steps(200);
    assert!(steps.iter().all(|ticks| ticks.len() == 10));
    for ticks in ticks_by_agent(&steps, 100) {
        assert_eq!(ticks.len(), 20);
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], 10);
        }
    }
    let stats = rig.app.world().resource::<AiTickStats>();
    assert!(stats.late_ticks_total > 0, "the overload is reported");
    assert_eq!(stats.max_late_steps, 9);
    assert_eq!(stats.deferred_last_step, 90);
}

#[test]
fn the_accumulated_dt_is_the_time_since_the_last_tick() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![tier(10.0, 1), tier(f32::MAX, 4)],
        focus: None,
    });
    rig.agent(0, 1.0);
    rig.agent(1, 50.0);
    let steps = rig.steps(13);
    let step = rig.step;
    let total = |id: i64| -> f32 {
        steps
            .iter()
            .flatten()
            .filter(|(i, _)| *i == id)
            .map(|(_, dt)| *dt)
            .sum()
    };
    let dts = |id: i64| -> Vec<f32> {
        steps
            .iter()
            .flatten()
            .filter(|(i, _)| *i == id)
            .map(|(_, dt)| *dt)
            .collect()
    };
    // Cadence 1 sees exactly the fixed step, as with no budget.
    assert!(dts(0).iter().all(|dt| dt.to_bits() == step.to_bits()));
    assert_eq!(dts(0).len(), 13);
    // Cadence 4: a first tick at once, then every 4 steps with 4 steps of time.
    assert_eq!(dts(1), vec![step, 4.0 * step, 4.0 * step, 4.0 * step]);
    assert!((total(1) - 13.0 * step).abs() < 1e-5, "no time is lost");
}

#[test]
fn a_tier_limit_holds_inside_the_step_limit() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![AiTickTier {
            max_distance: f32::MAX,
            cadence_steps: 1,
            max_trees_per_step: 3,
        }],
        focus: None,
    });
    for id in 0..9 {
        rig.agent(id, 1.0);
    }
    let steps = rig.steps(30);
    assert!(steps.iter().all(|ticks| ticks.len() == 3));
    for ticks in ticks_by_agent(&steps, 9) {
        assert_eq!(ticks.len(), 10);
    }
}

#[test]
fn always_on_ticks_every_step_and_counts_against_the_step_limit() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 5,
        tiers: vec![AiTickTier {
            max_distance: f32::MAX,
            cadence_steps: 40,
            max_trees_per_step: 1,
        }],
        focus: None,
    });
    for id in 0..40 {
        let agent = rig.agent(id, 1.0);
        if id == 20 || id == 39 {
            rig.app.world_mut().entity_mut(agent).insert(AiImportance {
                always_on: true,
                focus: None,
            });
        }
    }
    let steps = rig.steps(80);
    assert!(steps.iter().all(|ticks| ticks.len() <= 5));
    let by_agent = ticks_by_agent(&steps, 40);
    assert_eq!(by_agent[20].len(), 80);
    assert_eq!(by_agent[39].len(), 80);
}

#[test]
fn moving_nearer_shortens_the_cadence_and_a_lost_focus_means_the_last_tier() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![tier(10.0, 1), tier(f32::MAX, 8)],
        focus: None,
    });
    let agent = rig.agent(0, 100.0);
    let steps = rig.steps(8);
    assert_eq!(steps.iter().flatten().count(), 1, "only the first tick so far");
    let info = ai_tick_agent_info(rig.app.world(), agent).expect("a budget exists");
    assert_eq!(info.tier, 1);
    assert_eq!(info.distance, Some(100.0));

    rig.app
        .world_mut()
        .get_mut::<Transform>(agent)
        .expect("t")
        .translation = Vec3::new(1.0, 0.0, 0.0);
    let steps = rig.steps(4);
    assert_eq!(steps.iter().flatten().count(), 4, "cadence 1 from the next step");
    let (_, dt) = steps[0][0];
    assert!(
        (dt - 8.0 * rig.step).abs() < 1e-6,
        "with the time it skipped: {dt}"
    );

    let focus = rig.focus;
    rig.app.world_mut().despawn(focus);
    let steps = rig.steps(8);
    assert_eq!(steps.iter().flatten().count(), 1, "back to the last tier");
    assert_eq!(
        ai_tick_agent_info(rig.app.world(), agent).map(|i| i.distance),
        Some(None)
    );
}

#[test]
fn an_agent_own_focus_overrides_the_budget_focus() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![tier(10.0, 1), tier(f32::MAX, 8)],
        focus: None,
    });
    let own = rig
        .app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::new(500.0, 0.0, 0.0)))
        .id();
    let agent = rig.agent(0, 499.0);
    rig.app.world_mut().entity_mut(agent).insert(AiImportance {
        always_on: false,
        focus: Some(own),
    });
    let steps = rig.steps(5);
    assert_eq!(steps.iter().flatten().count(), 5);
}

#[test]
fn parked_agents_keep_no_clock_and_despawned_ones_free_their_turn() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 2,
        tiers: vec![tier(f32::MAX, 2)],
        focus: None,
    });
    let a = rig.agent(0, 1.0);
    let b = rig.agent(1, 1.0);
    rig.agent(2, 1.0);
    rig.agent(3, 1.0);
    rig.steps(4);
    rig.app.world_mut().entity_mut(a).insert(LayerParked {
        restore: regia_scene::prelude::Visibility::Inherited,
        inherited: false,
    });
    rig.app.world_mut().despawn(b);
    let steps = rig.steps(50);
    assert!(steps.iter().flatten().all(|(id, _)| *id >= 2));
    assert_eq!(rig.app.world().resource::<AiTickStats>().trees, 2);
    rig.app.world_mut().entity_mut(a).remove::<LayerParked>();
    let steps = rig.steps(2);
    let back: Vec<f32> = steps
        .iter()
        .flatten()
        .filter(|(id, _)| *id == 0)
        .map(|(_, dt)| *dt)
        .collect();
    assert_eq!(
        back,
        vec![rig.step],
        "it comes back with one step of time, not fifty"
    );
}

#[test]
fn a_late_spawn_gets_a_turn_within_its_cadence() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 4,
        tiers: vec![tier(f32::MAX, 10)],
        focus: None,
    });
    for id in 0..30 {
        rig.agent(id, 1.0);
    }
    rig.steps(25);
    rig.agent(30, 1.0);
    let steps = rig.steps(10);
    assert!(
        steps.iter().flatten().any(|(id, _)| *id == 30),
        "the new agent ticks within one cadence"
    );
}

#[test]
fn turning_the_budget_off_ticks_everything_again() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 1,
        tiers: vec![tier(f32::MAX, 5)],
        focus: None,
    });
    for id in 0..5 {
        rig.agent(id, 1.0);
    }
    rig.steps(7);
    rig.app.world_mut().resource_mut::<AiTickBudget>().enabled = false;
    let steps = rig.steps(3);
    assert!(steps.iter().all(|ticks| ticks.len() == 5));
    assert!(
        steps
            .iter()
            .flatten()
            .all(|(_, dt)| dt.to_bits() == rig.step.to_bits())
    );
    rig.app.world_mut().resource_mut::<AiTickBudget>().enabled = true;
    let steps = rig.steps(1);
    assert_eq!(steps[0].len(), 1, "on again, it starts afresh");
    assert_eq!(steps[0][0].1.to_bits(), rig.step.to_bits());
}

#[test]
fn behavior_and_state_trees_share_the_budget() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 3,
        tiers: vec![tier(f32::MAX, 4)],
        focus: None,
    });
    let def = StateTreeDef::from_ron(
        r#"(format_version: 2, root: "R", states: [(name: "R", tasks: ["test::Count"], transitions: [(trigger: Completed, target: State("R"))])])"#,
    )
    .expect("parses");
    for id in 0..6 {
        let agent = rig.agent(id, 1.0);
        let state_tree = {
            let registry = rig.app.world().resource::<BtTaskRegistry>();
            StateTree::compile(&def, registry).expect("compiles")
        };
        rig.app.world_mut().entity_mut(agent).insert(state_tree);
    }
    let steps = rig.steps(40);
    assert!(
        steps.iter().all(|ticks| ticks.len() <= 3 * 2),
        "a state tree tick may run its task once"
    );
    let stats = rig.app.world().resource::<AiTickStats>();
    assert_eq!(stats.trees, 12);
    assert!(stats.ticks_last_step <= 3);
    let statuses = {
        let world = rig.app.world_mut();
        world
            .query::<&StateTreeStatus>()
            .iter(world)
            .map(|status| status.ticks)
            .collect::<Vec<_>>()
    };
    assert_eq!(statuses.len(), 6);
    assert!(statuses.iter().all(|ticks| *ticks >= 8), "{statuses:?}");
    assert!(statuses.iter().all(|ticks| *ticks <= 11), "{statuses:?}");
}

#[test]
fn more_always_on_agents_than_the_limit_take_turns() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 2,
        tiers: vec![tier(f32::MAX, 100)],
        focus: None,
    });
    for id in 0..6 {
        let agent = rig.agent(id, 1.0);
        rig.app.world_mut().entity_mut(agent).insert(AiImportance {
            always_on: true,
            focus: None,
        });
    }
    let steps = rig.steps(30);
    assert!(steps.iter().all(|ticks| ticks.len() == 2));
    for ticks in ticks_by_agent(&steps, 6) {
        assert_eq!(ticks.len(), 10);
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], 3);
        }
    }
}

#[test]
fn always_on_agents_filling_the_limit_do_not_starve_the_rest() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 3,
        tiers: vec![tier(f32::MAX, 4)],
        focus: None,
    });
    for id in 0..3 {
        let agent = rig.agent(id, 1.0);
        rig.app.world_mut().entity_mut(agent).insert(AiImportance {
            always_on: true,
            focus: None,
        });
    }
    for id in 3..6 {
        rig.agent(id, 1.0);
    }
    let steps = rig.steps(120);
    assert!(steps.iter().all(|ticks| ticks.len() <= 3));
    let by_agent = ticks_by_agent(&steps, 6);
    for (id, ticks) in by_agent.iter().enumerate().skip(3) {
        assert!(ticks.len() > 10, "agent {id} ticked {} times", ticks.len());
    }
    assert!(
        by_agent[0].len() > by_agent[3].len(),
        "always-on still ticks most"
    );
    let stats = rig.app.world().resource::<AiTickStats>();
    assert!(
        stats.max_late_steps >= 4,
        "the waiting shows: {}",
        stats.max_late_steps
    );
}

#[test]
fn always_on_agents_do_not_use_up_a_tier_limit() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![AiTickTier {
            max_distance: f32::MAX,
            cadence_steps: 1,
            max_trees_per_step: 2,
        }],
        focus: None,
    });
    for id in 0..2 {
        let agent = rig.agent(id, 1.0);
        rig.app.world_mut().entity_mut(agent).insert(AiImportance {
            always_on: true,
            focus: None,
        });
    }
    for id in 2..4 {
        rig.agent(id, 1.0);
    }
    let steps = rig.steps(10);
    assert!(steps.iter().all(|ticks| ticks.len() == 4), "{steps:?}");
}

#[test]
fn finished_and_paused_state_trees_cost_no_budget() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 2,
        tiers: vec![tier(f32::MAX, 1)],
        focus: None,
    });
    // A state whose task succeeds with no transition finishes the tree on its first tick.
    let def = StateTreeDef::from_ron(
        r#"(format_version: 2, root: "R", states: [(name: "R", tasks: ["test::Count"])])"#,
    )
    .expect("parses");
    for id in 100..110 {
        let mut board = Blackboard::new();
        board.insert("id", id as i64);
        let tree = StateTree::compile(&def, rig.app.world().resource::<BtTaskRegistry>()).expect("compiles");
        rig.app.world_mut().spawn((tree, board));
    }
    // Two a step, so all ten have had their tick after five steps.
    rig.steps(6);
    let finished = rig.app.world().resource::<AiTickStats>().ticks_last_step;
    assert_eq!(finished, 0, "ten finished trees use none of the two slots");
    // Waiting for their one tick on two slots made some of them late; that part is over.
    let late_before = rig.app.world().resource::<AiTickStats>().late_ticks_total;
    for id in 0..2 {
        rig.agent(id, 1.0);
    }
    let steps = rig.steps(5);
    assert!(steps.iter().all(|ticks| ticks.len() == 2), "{steps:?}");
    assert_eq!(
        rig.app.world().resource::<AiTickStats>().late_ticks_total,
        late_before
    );
}

#[test]
fn a_hold_lasts_its_seconds_when_the_cadence_changes() {
    let mut rig = rig(AiTickBudget {
        enabled: true,
        max_trees_per_step: 0,
        tiers: vec![tier(10.0, 1), tier(f32::MAX, 16)],
        focus: None,
    });
    // Holds for 1 s (60 steps), then moves on.
    let def = StateTreeDef::from_ron(
        r#"(format_version: 2, root: "R", states: [
            (name: "R", children: ["Wait", "Done"]),
            (name: "Wait", transitions: [(trigger: Tick, hold: 1.0, target: State("Done"))]),
            (name: "Done"),
        ])"#,
    )
    .expect("parses");
    let tree = StateTree::compile(&def, rig.app.world().resource::<BtTaskRegistry>()).expect("compiles");
    let agent = rig
        .app
        .world_mut()
        .spawn((
            tree,
            Blackboard::new(),
            Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
        ))
        .id();
    // Far for 48 steps, then near: about 12 more steps, not another 60.
    rig.steps(48);
    let active = |rig: &Rig| {
        rig.app
            .world()
            .get::<StateTreeStatus>(agent)
            .expect("status")
            .active_path
            .join("/")
    };
    assert_eq!(active(&rig), "R/Wait");
    rig.app
        .world_mut()
        .get_mut::<Transform>(agent)
        .expect("t")
        .translation = Vec3::ZERO;
    let mut left = 0;
    while active(&rig) != "R/Done" && left < 100 {
        rig.steps(1);
        left += 1;
    }
    assert!((10..=30).contains(&left), "it took {left} more steps");
}
