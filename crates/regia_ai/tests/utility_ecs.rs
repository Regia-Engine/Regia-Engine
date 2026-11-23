// SPDX-License-Identifier: MPL-2.0
//! `UtilityPlugin` end to end (ADR-2565): fixed steps only, no wall clock.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_ai::Blackboard;
use regia_ai::utility::{
    ActionDef, ActionScore, ConsiderationDef, DecisionReport, ResponseCurve, SelectionMode, UtilityAiDef,
    migrate_ron, utility_ai_migration_sources, utility_ai_schema_fingerprint,
};
use regia_ai::utility_ecs::{
    UtilityClock, UtilityDefs, UtilityPlugin, UtilityReasoner, UtilityStep, load_utility_definition,
};
use regia_app::prelude::*;
use regia_asset::{AssetServer, AssetServerSettings, SourceAssetChanged, SourceKind, current_platform};

/// A fresh directory for this process and call (`regia_test_utils::unique_temp_dir` would pull the whole
/// engine into this crate's test build).
fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("regia-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the temp dir");
    dir
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, UtilityPlugin));
    regia_app::enable_fixed_step(&mut app, 60.0);
    app
}

fn step(app: &mut App, n: u64) {
    let goal = app.world().resource::<UtilityClock>().tick + n;
    for _ in 0..(n + 2) * 64 {
        if app.world().resource::<UtilityClock>().tick >= goal {
            return;
        }
        app.update();
    }
    panic!("utility did not reach tick {goal}");
}

fn action(name: &str, key: &str) -> ActionDef {
    ActionDef {
        name: name.to_string(),
        considerations: vec![ConsiderationDef {
            key: key.to_string(),
            min: 0.0,
            max: 100.0,
            curve: ResponseCurve::default(),
        }],
        weight: 1.0,
        cooldown: 0.0,
        momentum: 0.0,
    }
}

fn def() -> UtilityAiDef {
    UtilityAiDef {
        actions: vec![action("attack", "health"), action("flee", "threat")],
        ..UtilityAiDef::default()
    }
}

fn board(health: f32, threat: f32) -> Blackboard {
    let mut board = Blackboard::new();
    board.insert("health", health);
    board.insert("threat", threat);
    board
}

fn spawn(app: &mut App, reasoner: UtilityReasoner, blackboard: Blackboard) -> Entity {
    let mut reasoner = reasoner;
    reasoner.interval_ticks = 1;
    app.world_mut().spawn((reasoner, blackboard)).id()
}

fn reasoner(app: &App, agent: Entity) -> &UtilityReasoner {
    app.world().get::<UtilityReasoner>(agent).expect("reasoner")
}

fn blackboard(app: &App, agent: Entity) -> &Blackboard {
    app.world().get::<Blackboard>(agent).expect("blackboard")
}

/// Acceptance: a utility reasoner picks the expected action, and the choice follows the blackboard.
#[test]
fn a_reasoner_picks_the_expected_action_and_follows_the_blackboard() {
    let mut app = app();
    let agent = spawn(
        &mut app,
        UtilityReasoner::from_def(&def()).expect("valid"),
        board(90.0, 10.0),
    );
    step(&mut app, 1);
    assert_eq!(reasoner(&app, agent).current_action, "attack");
    assert_eq!(blackboard(&app, agent).get_string("utility_action"), "attack");
    assert_eq!(reasoner(&app, agent).decisions, 1);

    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("blackboard")
        .insert("threat", 95.0f32);
    step(&mut app, 1);
    assert_eq!(reasoner(&app, agent).current_action, "flee");
    assert_eq!(blackboard(&app, agent).get_string("utility_action"), "flee");
}

/// The report minus its clock: scores do not depend on the clock without cooldowns.
fn scores(report: &DecisionReport) -> (Option<&str>, &[ActionScore]) {
    (report.selected.as_deref(), &report.actions)
}

/// Acceptance: explain returns the same scores the decision used, per consideration, and changes
/// nothing.
#[test]
fn explain_returns_the_same_scores_as_the_decision_and_changes_nothing() {
    let mut def = def();
    def.actions[0].momentum = 0.25;
    def.actions[1].considerations.push(ConsiderationDef {
        key: "ammo".to_string(),
        min: 0.0,
        max: 10.0,
        curve: ResponseCurve::default(),
    });
    let mut app = app();
    let mut facts = board(40.0, 60.0);
    facts.insert("ammo", 5.0f32);
    let agent = spawn(&mut app, UtilityReasoner::from_def(&def).expect("valid"), facts);
    app.world_mut()
        .get_mut::<UtilityReasoner>(agent)
        .expect("r")
        .interval_ticks = 3;
    // The phase is the reasoner's spawn-order arrival (the engine's stagger, ADR-4651); the
    // entity index is only the fallback for a reasoner the tracking never saw.
    let offset = app
        .world()
        .get_resource::<regia_ecs::spawn_order::SpawnOrder<UtilityReasoner>>()
        .and_then(|order| order.get(agent))
        .unwrap_or_else(|| u64::from(agent.index_u32()));
    // Run to the step before this agent's decision.
    while !(app.world().resource::<UtilityClock>().tick + offset + 1).is_multiple_of(3) {
        step(&mut app, 1);
    }
    let decisions = reasoner(&app, agent).decisions;
    let now = app.world().resource::<UtilityClock>().now();
    let before = reasoner(&app, agent)
        .explain(blackboard(&app, agent), now)
        .expect("a reasoner");
    assert_eq!(
        reasoner(&app, agent).decisions,
        decisions,
        "explain takes no decision"
    );
    step(&mut app, 1);
    let view = reasoner(&app, agent);
    assert_eq!(view.decisions, decisions + 1, "the decision came");
    let taken = view.last_report().expect("a report");
    assert_eq!(scores(&before), scores(taken));
    let flee = taken.actions.iter().find(|a| a.name == "flee").expect("flee");
    assert_eq!(flee.considerations.len(), 2, "per consideration");
    assert_eq!(flee.considerations[0].key, "threat");
    assert_eq!(flee.considerations[0].raw_input, 60.0);
    assert_eq!(flee.considerations[1].key, "ammo");
    assert_eq!(flee.considerations[1].normalized, 0.5);
}

/// The rate is configurable: a decision every `interval_ticks` steps.
#[test]
fn the_decision_rate_is_configurable_and_a_crowd_is_staggered() {
    let mut app = app();
    let agents: Vec<Entity> = (0..12)
        .map(|_| {
            let mut r = UtilityReasoner::from_def(&def()).expect("valid");
            r.interval_ticks = 4;
            app.world_mut().spawn((r, board(90.0, 10.0))).id()
        })
        .collect();
    let mut most_at_once = 0;
    for _ in 0..40 {
        step(&mut app, 1);
        most_at_once = most_at_once.max(app.world().resource::<UtilityClock>().decisions_last_tick);
    }
    for &agent in &agents {
        assert_eq!(reasoner(&app, agent).decisions, 10, "40 steps / 4");
    }
    assert!(most_at_once < 12, "staggered: {most_at_once} in one step");
}

/// Without a blackboard nothing is scored, and nothing panics.
#[test]
fn an_agent_without_a_blackboard_is_not_scored() {
    let mut app = app();
    let mut r = UtilityReasoner::from_def(&def()).expect("valid");
    r.interval_ticks = 1;
    let agent = app.world_mut().spawn(r).id();
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).decisions, 0);
}

fn project(tag: &str) -> (App, std::path::PathBuf) {
    let root = unique_temp_dir(tag);
    std::fs::create_dir_all(root.join("assets/ai")).expect("dir");
    let server = AssetServer::open(AssetServerSettings {
        project_root: root.clone(),
        platform: current_platform(),
        content_dir: None,
    })
    .expect("open");
    let mut app = app();
    app.insert_resource(server);
    app.add_message::<SourceAssetChanged>();
    (app, root)
}

const ATTACK_FIRST: &str = r#"(
    format_version: 1,
    actions: [
        (name: "attack", considerations: [(key: "health", min: 0.0, max: 100.0)], weight: 2.0),
        (name: "flee", considerations: [(key: "threat", min: 0.0, max: 100.0)]),
    ],
)"#;

const FLEE_FIRST: &str = r#"(
    format_version: 1,
    actions: [
        (name: "attack", considerations: [(key: "health", min: 0.0, max: 100.0)]),
        (name: "flee", considerations: [(key: "threat", min: 0.0, max: 100.0)], weight: 9.0),
    ],
)"#;

fn write_atomic(path: &Path, text: &str) {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, text).expect("write");
    std::fs::rename(&temp, path).expect("rename");
}

/// The asset route and hot reload: the file loads, an edit applies, a broken edit keeps what was
/// running and says why, and the fix applies. The agent keeps its cooldowns through a reload.
#[test]
fn a_utility_asset_loads_and_hot_reloads_and_a_bad_edit_keeps_the_old_one() {
    let (mut app, root) = project("utility-asset");
    let file = root.join("assets/ai/guard.utility.ron");
    write_atomic(&file, ATTACK_FIRST);
    let agent = spawn(
        &mut app,
        UtilityReasoner::from_path("ai/guard.utility.ron"),
        board(50.0, 50.0),
    );
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).current_action, "attack");
    assert_eq!(reasoner(&app, agent).generation, 1);

    write_atomic(&file, FLEE_FIRST);
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).current_action, "flee", "the edit applied");
    assert_eq!(reasoner(&app, agent).generation, 2);

    write_atomic(&file, "( not ron");
    app.world_mut()
        .write_message(SourceAssetChanged::new(file.clone()));
    step(&mut app, 2);
    assert_eq!(
        reasoner(&app, agent).current_action,
        "flee",
        "the old definition runs on"
    );
    assert_eq!(reasoner(&app, agent).generation, 2);
    let error = app
        .world()
        .resource::<UtilityDefs>()
        .slot("ai/guard.utility.ron")
        .and_then(|slot| slot.error.clone())
        .expect("the failure is recorded");
    assert_eq!(error.reason, "parse");

    write_atomic(&file, ATTACK_FIRST);
    app.world_mut().write_message(SourceAssetChanged::new(file));
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).current_action, "attack", "fixed");
    let slot = app
        .world()
        .resource::<UtilityDefs>()
        .slot("ai/guard.utility.ron")
        .cloned()
        .expect("slot");
    assert_eq!(slot.error, None);
    assert_eq!(slot.generation, 3);
    std::fs::remove_dir_all(&root).ok();
}

/// A change to some other file reloads nothing.
#[test]
fn a_change_to_another_file_reloads_nothing() {
    let (mut app, root) = project("utility-other");
    let file = root.join("assets/ai/guard.utility.ron");
    write_atomic(&file, ATTACK_FIRST);
    let agent = spawn(
        &mut app,
        UtilityReasoner::from_path("ai/guard.utility.ron"),
        board(50.0, 50.0),
    );
    step(&mut app, 2);
    write_atomic(&file, FLEE_FIRST);
    app.world_mut()
        .write_message(SourceAssetChanged::new(root.join("assets/ai/other.utility.ron")));
    app.world_mut().write_message(SourceAssetChanged::new(
        root.join("assets/ai/guard.statetree.ron"),
    ));
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).generation, 1);
    assert_eq!(reasoner(&app, agent).current_action, "attack");
    std::fs::remove_dir_all(&root).ok();
}

/// Missing files and escapes are reported, not panics, and a later file is picked up by a reload.
#[test]
fn a_bad_path_or_missing_file_reports_and_waits() {
    let (mut app, root) = project("utility-missing");
    let agent = spawn(
        &mut app,
        UtilityReasoner::from_path("ai/late.utility.ron"),
        board(50.0, 50.0),
    );
    let escape = spawn(
        &mut app,
        UtilityReasoner::from_path("../outside.utility.ron"),
        board(50.0, 50.0),
    );
    step(&mut app, 2);
    assert!(reasoner(&app, agent).reasoner().is_none());
    assert_eq!(reasoner(&app, agent).decisions, 0);
    let defs = app.world().resource::<UtilityDefs>();
    let reason = |path: &str| defs.slot(path).and_then(|s| s.error.clone()).map(|e| e.reason);
    assert_eq!(reason("ai/late.utility.ron"), Some("definition_not_found"));
    assert_eq!(reason("../outside.utility.ron"), Some("bad_path"));

    let file = root.join("assets/ai/late.utility.ron");
    write_atomic(&file, ATTACK_FIRST);
    app.world_mut().write_message(SourceAssetChanged::new(file));
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).current_action, "attack");
    assert!(reasoner(&app, escape).reasoner().is_none());
    std::fs::remove_dir_all(&root).ok();
}

/// Invalid content is the format's own reason, for the agent's `data.reason`.
#[test]
fn a_definition_that_does_not_validate_names_the_reason() {
    let (mut app, root) = project("utility-invalid");
    write_atomic(
        &root.join("assets/ai/bad.utility.ron"),
        r#"(format_version: 1, actions: [])"#,
    );
    write_atomic(
        &root.join("assets/ai/future.utility.ron"),
        r#"(format_version: 99, actions: [])"#,
    );
    let world = app.world_mut();
    world.init_resource::<UtilityDefs>();
    assert!(!load_utility_definition(world, "ai/bad.utility.ron"));
    assert!(!load_utility_definition(world, "ai/future.utility.ron"));
    let defs = world.resource::<UtilityDefs>();
    let reason = |path: &str| defs.slot(path).and_then(|s| s.error.clone()).map(|e| e.reason);
    assert_eq!(reason("ai/bad.utility.ron"), Some("empty"));
    assert_eq!(reason("ai/future.utility.ron"), Some("unsupported_version"));
    std::fs::remove_dir_all(&root).ok();
}

/// A hot reload keeps state matched by name; a cooldown carries over so tuning a number does not
/// reset the agent.
#[test]
fn a_reload_keeps_the_current_action_and_cooldowns_by_name() {
    let mut first = def();
    first.actions[0].cooldown = 100.0;
    let mut reasoner_a = regia_ai::Reasoner::new(&first).expect("valid");
    let facts = board(90.0, 10.0);
    reasoner_a.decide(&facts, 1.0);
    assert_eq!(reasoner_a.current_action(), Some("attack"));
    let remaining = reasoner_a.cooldown_remaining("attack", 1.0);

    let mut second = def();
    second.actions[0].cooldown = 100.0;
    second.actions[1].weight = 3.0;
    second.actions.push(action("hide", "threat"));
    let mut reasoner_b = regia_ai::Reasoner::new(&second).expect("valid");
    reasoner_b.adopt_state_from(&reasoner_a);
    assert_eq!(reasoner_b.current_action(), Some("attack"));
    assert_eq!(reasoner_b.cooldown_remaining("attack", 1.0), remaining);
    assert_eq!(reasoner_b.cooldown_remaining("hide", 1.0), Some(0.0));

    // Renaming the current action: nothing to keep.
    let mut third = def();
    third.actions[0].name = "charge".to_string();
    let mut reasoner_c = regia_ai::Reasoner::new(&third).expect("valid");
    reasoner_c.adopt_state_from(&reasoner_a);
    assert_eq!(reasoner_c.current_action(), None);
}

/// Determinism: two runs give the same reports, including weighted draws.
#[test]
fn two_runs_decide_the_same() {
    fn run() -> Vec<(String, Option<String>)> {
        let mut def = def();
        def.seed = 42;
        def.selection = SelectionMode::WeightedRandom {
            top_n: 2,
            margin: 1.0,
        };
        let mut app = app();
        let agent = spawn(
            &mut app,
            UtilityReasoner::from_def(&def).expect("valid"),
            board(50.0, 50.0),
        );
        let mut trace = Vec::new();
        for tick in 0..120u32 {
            app.world_mut()
                .get_mut::<Blackboard>(agent)
                .expect("blackboard")
                .insert("threat", (tick % 100) as f32);
            step(&mut app, 1);
            let view = reasoner(&app, agent);
            trace.push((
                view.current_action.clone(),
                view.last_report().map(|r| format!("{r:?}")),
            ));
        }
        trace
    }
    let (a, b) = (run(), run());
    assert_eq!(a, b);
    let both = a.iter().any(|(name, _)| name == "attack") && a.iter().any(|(name, _)| name == "flee");
    assert!(both, "the draw picked both over the run");
}

/// The frozen v1 shape: the fingerprint names every nested struct and every spelling, the documented
/// example parses and validates, and the migrator is the identity at v1.
#[test]
fn the_frozen_format_is_covered() {
    let fingerprint = utility_ai_schema_fingerprint();
    for needle in [
        "format_version",
        "selection",
        "considerations",
        "momentum",
        "curve",
        "x_shift",
        "points",
        "WeightedRandom",
        "top_n",
        "margin",
        "Piecewise",
    ] {
        assert!(fingerprint.contains(needle), "{needle} is not in {fingerprint}");
    }
    assert_eq!(utility_ai_migration_sources(), Vec::<u32>::new());
    let doc = include_str!("../../../docs/formats/utility.example.ron");
    let parsed = UtilityAiDef::from_ron(doc).expect("the documented example parses");
    regia_ai::Reasoner::new(&parsed).expect("the documented example validates");
    assert_eq!(migrate_ron(doc).expect("identity at v1"), doc);
}

/// The dev watcher forwards `.utility.ron` changes: without this the hot reload never hears of an edit.
#[test]
fn the_watcher_treats_a_utility_file_as_read_at_runtime() {
    let kind = SourceKind::of(Path::new("assets/ai/guard.utility.ron"));
    assert_eq!(kind, SourceKind::GameplayData);
    assert!(kind.is_read_at_runtime());
    assert!(!kind.is_cooked(), "it ships raw, as the format page says");
}

/// The reflected config survives a scene round trip; the runtime state is not part of it.
#[test]
fn the_component_survives_the_reflection_path_without_its_runtime_state() {
    use regia_ecs::reflect::serde::{TypedReflectDeserializer, TypedReflectSerializer};
    use regia_ecs::reflect::{AppTypeRegistry, FromReflect};
    use serde::de::DeserializeSeed;

    let app = app();
    let mut original = UtilityReasoner::from_def(&def()).expect("valid");
    original.path = "ai/guard.utility.ron".to_string();
    original.interval_ticks = 9;
    let registry = app.world().resource::<AppTypeRegistry>().read();
    let text = ron::to_string(&TypedReflectSerializer::new(&original, &registry)).expect("serialize");
    let registration = registry
        .get(std::any::TypeId::of::<UtilityReasoner>())
        .expect("registered");
    let mut deserializer = ron::Deserializer::from_str(&text).expect("text");
    let dynamic = TypedReflectDeserializer::new(registration, &registry)
        .deserialize(&mut deserializer)
        .expect("deserialize");
    let loaded = UtilityReasoner::from_reflect(dynamic.as_ref()).expect("from reflect");
    assert_eq!(loaded.path, "ai/guard.utility.ron");
    assert_eq!(loaded.interval_ticks, 9);
    assert!(
        loaded.reasoner().is_none(),
        "the next sync builds it from the file"
    );
}

/// A reload that removes the current action clears the published name: a tree must not keep running an
/// action that no longer exists.
#[test]
fn a_reload_that_removes_the_current_action_clears_its_name() {
    let (mut app, root) = project("utility-removed");
    let file = root.join("assets/ai/guard.utility.ron");
    write_atomic(&file, ATTACK_FIRST);
    let agent = spawn(
        &mut app,
        UtilityReasoner::from_path("ai/guard.utility.ron"),
        board(50.0, 50.0),
    );
    step(&mut app, 2);
    assert_eq!(blackboard(&app, agent).get_string("utility_action"), "attack");

    // Now only an action with no considerations: it is never selected, so nothing is eligible.
    write_atomic(
        &file,
        r#"(format_version: 1, actions: [(name: "hide", considerations: [])])"#,
    );
    app.world_mut().write_message(SourceAssetChanged::new(file));
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).generation, 2);
    assert_eq!(reasoner(&app, agent).current_action, "", "attack is gone");
    assert_eq!(
        blackboard(&app, agent).get("utility_action"),
        None,
        "and so is its key"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The first decision of a reasoner spawned during the fixed steps comes on the same step however many
/// fixed steps the frame has.
#[test]
fn the_first_decision_does_not_depend_on_steps_per_frame() {
    #[derive(Resource)]
    struct Spawn(Option<(UtilityReasoner, Blackboard)>);

    fn spawner(mut commands: Commands, mut spawn: ResMut<Spawn>, mut seen: ResMut<Agent>) {
        if let Some(bundle) = spawn.0.take() {
            seen.0 = Some(commands.spawn(bundle).id());
        }
    }

    #[derive(Resource, Default)]
    struct Agent(Option<Entity>);

    fn run(steps_per_frame: u32) -> u64 {
        let (mut app, root) = project("utility-first");
        write_atomic(&root.join("assets/ai/guard.utility.ron"), ATTACK_FIRST);
        {
            let mut fixed = app.world_mut().resource_mut::<FixedTime>();
            fixed.hz = 60.0 * f64::from(steps_per_frame);
            fixed.max_substeps = steps_per_frame;
        }
        let mut reasoner = UtilityReasoner::from_path("ai/guard.utility.ron");
        reasoner.interval_ticks = 1;
        app.insert_resource(Spawn(Some((reasoner, board(50.0, 50.0)))));
        app.init_resource::<Agent>();
        app.add_systems(FixedUpdate, spawner.before(UtilityStep));
        // Runs as many steps in each case, with the frame boundaries in different places.
        while app.world().resource::<UtilityClock>().tick < 12 {
            app.update();
        }
        let agent = app.world().resource::<Agent>().0.expect("spawned");
        let clock = app.world().resource::<UtilityClock>().tick;
        let decisions = app
            .world()
            .get::<UtilityReasoner>(agent)
            .expect("reasoner")
            .decisions;
        std::fs::remove_dir_all(&root).ok();
        clock - decisions
    }
    assert_eq!(run(1), 0, "a decision on every step from the first");
    assert_eq!(run(3), 0);
}

/// Finding 3 (audit G): the decision phase came from the entity index, so two runs of the same
/// level — which can allocate different ids to the same spawns — decided on different steps and
/// saw different blackboard values. The phase comes from the reasoner's spawn-order arrival.
#[test]
fn two_reasoners_decide_on_arrival_phase_when_ids_are_swapped() {
    /// Decision ticks per arrival (true: the reasoner inserted first), for 24 fixed steps.
    fn run_once(eat_id: bool) -> Vec<(bool, Vec<u64>, String)> {
        let mut app = app();
        if eat_id {
            app.world_mut().spawn_empty();
        }
        let first = app.world_mut().spawn_empty().id();
        let second = app.world_mut().spawn_empty().id();
        // Arrival [second, first] in every run; `eat_id` shifts both ids by one.
        for holder in [second, first] {
            let mut reasoner = UtilityReasoner::from_def(&def()).expect("valid");
            reasoner.interval_ticks = 6;
            app.world_mut()
                .entity_mut(holder)
                .insert((reasoner, board(50.0, 50.0)));
        }
        let mut seen: Vec<(bool, Vec<u64>, String)> = vec![
            (true, Vec::new(), String::new()),
            (false, Vec::new(), String::new()),
        ];
        let mut decided = [0u64, 0u64];
        for tick in 1..=24u64 {
            step(&mut app, 1);
            for (slot, holder) in [(0usize, second), (1usize, first)] {
                let component = reasoner(&app, holder);
                if component.decisions > decided[slot] {
                    decided[slot] = component.decisions;
                    seen[slot].1.push(tick);
                    seen[slot].2 = component.current_action.clone();
                }
            }
        }
        seen
    }
    let plain = run_once(false);
    let shifted = run_once(true);
    assert_eq!(
        plain, shifted,
        "the arrival phases must not follow the entity ids"
    );
    assert!(
        !plain[0].1.is_empty() && !plain[1].1.is_empty(),
        "both reasoners decided: {plain:?}"
    );
}

/// Finding 14: a change reloads only the slot whose file it is. A file of the same name in another
/// folder is a different file, and a slot still waiting for its file is read again only when that
/// file changes, not on every `.utility.ron` change.
#[test]
fn a_change_reloads_only_the_slot_whose_file_changed() {
    let (mut app, root) = project("utility-exact");
    std::fs::create_dir_all(root.join("assets/bosses")).expect("dir");
    write_atomic(&root.join("assets/guard.utility.ron"), ATTACK_FIRST);
    let agent = spawn(
        &mut app,
        UtilityReasoner::from_path("guard.utility.ron"),
        board(50.0, 50.0),
    );
    let waiting = spawn(
        &mut app,
        UtilityReasoner::from_path("ai/late.utility.ron"),
        board(50.0, 50.0),
    );
    step(&mut app, 2);
    assert_eq!(reasoner(&app, agent).generation, 1);
    assert!(reasoner(&app, waiting).reasoner().is_none());

    // The waiting file appears without a change of its own; another folder's guard changes.
    write_atomic(&root.join("assets/ai/late.utility.ron"), FLEE_FIRST);
    let boss = root.join("assets/bosses/guard.utility.ron");
    write_atomic(&boss, FLEE_FIRST);
    app.world_mut().write_message(SourceAssetChanged::new(boss));
    step(&mut app, 2);
    let defs = app.world().resource::<UtilityDefs>();
    assert_eq!(
        defs.slot("guard.utility.ron").map(|slot| slot.generation),
        Some(1),
        "assets/guard.utility.ron did not change"
    );
    assert_eq!(reasoner(&app, agent).generation, 1);
    assert_eq!(reasoner(&app, agent).current_action, "attack");
    assert!(
        defs.slot("ai/late.utility.ron")
            .is_some_and(|slot| slot.definition.is_none()),
        "the waiting slot is not read for another file's change"
    );
    assert!(reasoner(&app, waiting).reasoner().is_none());

    app.world_mut()
        .write_message(SourceAssetChanged::new(root.join("assets/ai/late.utility.ron")));
    step(&mut app, 2);
    assert_eq!(
        reasoner(&app, waiting).current_action,
        "flee",
        "its own change loads it"
    );
    assert_eq!(reasoner(&app, agent).generation, 1);
    std::fs::remove_dir_all(&root).ok();
}
