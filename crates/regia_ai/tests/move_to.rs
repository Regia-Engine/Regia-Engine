// SPDX-License-Identifier: MPL-2.0
//! The `PathProvider` seam and the `MoveTo` leaf (ADR-8293): one leaf that plans through a named
//! provider (the f32 navmesh with its crowd, the deterministic grid, the deterministic navmesh),
//! follows the path, succeeds inside its acceptance radius, fails cleanly on an unreachable goal
//! and re-plans when its target moves. Fixed steps, no wall clock.

use std::sync::Arc;

use regia_ai::blackboard::{BbValue, Blackboard};
use regia_ai::path_provider::{
    CrowdSteered, DetGridProvider, DetNavMeshProvider, MOVE_TO_TASK, MoveTarget, MoveTo, MoveToState,
    MoveToStatus, NavMeshProvider, PartialReason, PathAgentSpec, PathFailure, PathProvider, PathProviders,
    PathRequestOutcome,
};
use regia_ai::state_tree::{StateTree, StateTreeDef, StateTreeRunStatus};
use regia_ai::{
    AiPlugin, AiStep, BehaviorTree, BehaviorTreeBuilder, BehaviorTreeStatus, BtStatus, BtTaskRegistry,
};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::schedule::{Schedules, SingleThreadedExecutor};
use regia_nav::det::grid::{GridSettings, HeightCells, NavGrid, NavStatic, Profile};
use regia_nav::det::navmesh::{BakeParts, GroundRaster, NavMesh as DetNavMesh};
use regia_nav::{
    CrowdAgentParams, NavCrowd, NavCrowdConfig, NavCrowdPlugin, NavCrowdStep, NavMesh, NavMeshBuilder,
};
use regia_scene::{GlobalTransform, Transform};

const TASK: &str = "move_to_test::MoveTo";

// ------------------------------------------------------------------------------------ fixtures

/// Two triangles covering the XZ square `[x0, x1] x [z0, z1]` at height 0.
fn quad(x0: f32, x1: f32, z0: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
        ],
        [
            Vec3::new(x0, 0.0, z0),
            Vec3::new(x1, 0.0, z1),
            Vec3::new(x0, 0.0, z1),
        ],
    ]
}

/// A 40 m square floor centered on the origin.
fn floor_mesh() -> NavMesh {
    NavMeshBuilder::default()
        .build(&quad(-20.0, 20.0, -20.0, 20.0))
        .expect("the floor builds")
}

/// Two 15 m islands with a 10 m gap: nothing on the left reaches the right.
fn islands_mesh() -> NavMesh {
    let mut tris = Vec::new();
    tris.extend(quad(-20.0, -5.0, -7.5, 7.5));
    tris.extend(quad(5.0, 20.0, -7.5, 7.5));
    NavMeshBuilder::default().build(&tris).expect("the islands build")
}

/// A 640 m deterministic grid (5 m cells) from a height function of world (x, z).
fn det_grid(height: impl Fn(f32, f32) -> f32) -> NavGrid {
    let (side, cell) = (128usize, 5.0f32);
    let half = side as f32 * cell * 0.5;
    let mut heights = Vec::with_capacity(side * side);
    for z in 0..side {
        for x in 0..side {
            heights.push(height(x as f32 * cell - half, z as f32 * cell - half));
        }
    }
    let ground = HeightCells::centered(side, side, cell, heights);
    let settings = Arc::new(GridSettings::hover_ground_jet());
    NavGrid::new(Arc::new(
        NavStatic::build(Some(&ground), settings).expect("ground"),
    ))
    .expect("grid")
}

/// A 60 m wall along x = 0 from z = -100 to z = 100: a route from the west to the east goes round.
fn walled_grid() -> NavGrid {
    det_grid(|x, z| {
        if x.abs() <= 7.5 && z.abs() <= 100.0 {
            60.0
        } else {
            0.0
        }
    })
}

/// A ring of cliff round the origin: the center cannot be reached from outside.
fn ringed_grid() -> NavGrid {
    det_grid(|x, z| {
        let r = (x * x + z * z).sqrt();
        if (40.0..=55.0).contains(&r) { 80.0 } else { 0.0 }
    })
}

/// A 40 m by 30 m walkable deterministic navmesh from a 5 m raster.
fn det_navmesh() -> DetNavMesh {
    let parts = BakeParts {
        ground: Some(GroundRaster {
            cols: 8,
            rows: 6,
            step_m: 5.0,
            origin_x: 0.0,
            origin_z: 0.0,
            walk: vec![true; 48],
            height: vec![0.0; 48],
        }),
        extent_m: 40.0,
        ..BakeParts::default()
    };
    DetNavMesh::bake(&parts).expect("a walkable raster bakes")
}

/// An app with the AI plugin on one-thread fixed steps (60 Hz), the move leaf registered as `TASK`.
fn app_with(leaf: MoveTo, extra: impl FnOnce(&mut App)) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    extra(&mut app);
    leaf.register(&mut app.world_mut().resource_mut::<BtTaskRegistry>(), TASK);
    app.update();
    let label = FixedUpdate.intern();
    if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
        schedule.set_executor(SingleThreadedExecutor::new());
    }
    app
}

fn one_leaf_tree(app: &App) -> BehaviorTree {
    let mut builder = BehaviorTreeBuilder::new();
    let leaf = builder.leaf("go", TASK);
    let def = builder.build(leaf);
    BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles")
}

fn spawn_agent(app: &mut App, at: Vec3, board: Blackboard) -> Entity {
    let transform = Transform::from_translation(at);
    app.world_mut()
        .spawn((transform, GlobalTransform::IDENTITY * transform, board))
        .id()
}

fn step(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
}

fn position(app: &App, agent: Entity) -> Vec3 {
    app.world()
        .get::<Transform>(agent)
        .expect("transform")
        .translation
}

fn bt_status(app: &App, agent: Entity) -> Option<BtStatus> {
    app.world()
        .get::<BehaviorTreeStatus>(agent)
        .map(|s| s.last_status)
}

/// Steps until the tree's root finishes (or `max` steps); returns the steps and the status.
fn run_tree(app: &mut App, agent: Entity, max: u32) -> (u32, BtStatus) {
    for n in 1..=max {
        step(app);
        if let Some(status) = bt_status(app, agent)
            && status != BtStatus::Running
        {
            return (n, status);
        }
    }
    (max, BtStatus::Running)
}

fn bb_string(app: &App, agent: Entity, key: &str) -> String {
    match app.world().get::<Blackboard>(agent).and_then(|b| b.get(key)) {
        Some(BbValue::String(s)) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn xz_dist(a: Vec3, b: Vec3) -> f32 {
    let (dx, dz) = (a.x - b.x, a.z - b.z);
    (dx * dx + dz * dz).sqrt()
}

// ------------------------------------------------------------------------------- acceptance

/// One `MoveTo` leaf on the f32 navmesh hands its path to the crowd: the crowd agent walks, the
/// leaf copies its position onto the entity and succeeds inside the acceptance radius.
#[test]
fn move_to_reaches_a_target_on_the_navmesh() {
    let target = Vec3::new(15.0, 0.0, 12.0);
    let start = Vec3::new(-15.0, 0.0, -12.0);
    let leaf = MoveTo::new(MoveTarget::Point(target), 0.5).with_provider("navmesh");
    let mut app = app_with(leaf, |app| {
        app.add_plugins(NavCrowdPlugin);
        app.configure_sets(FixedUpdate, (AiStep, NavCrowdStep).chain());
    });
    let mesh = floor_mesh();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("crowd");
    let id = crowd
        .add_agent(&mesh, start, CrowdAgentParams::new(0.35, 1.8, 3.5, 12.0))
        .expect("on the mesh");
    app.world_mut().insert_resource(mesh);
    app.world_mut().insert_resource(crowd);
    let agent = spawn_agent(&mut app, start, Blackboard::new());
    let tree = one_leaf_tree(&app);
    app.world_mut().entity_mut(agent).insert((tree, CrowdSteered(id)));

    let (steps, status) = run_tree(&mut app, agent, 1200);
    assert_eq!(
        status,
        BtStatus::Success,
        "after {steps} steps: {}",
        bb_string(&app, agent, "move_to_error")
    );
    let at = position(&app, agent);
    assert!(xz_dist(at, target) <= 0.5 + 1e-3, "ends at {at}, target {target}");
    // 33.9 m at 3.5 m/s is about 580 steps: the crowd walked, it did not teleport.
    assert!(steps > 400, "took {steps} steps");
    assert_eq!(bb_string(&app, agent, "move_to_state"), "arrived");
    let status = app
        .world()
        .get::<MoveToStatus>(agent)
        .expect("progress component");
    let report = status.entry("move_to").expect("the leaf's report");
    assert_eq!(report.state, MoveToState::Arrived);
    assert_eq!(report.provider, "navmesh");
    assert!(report.crowd, "the navmesh provider hands the path to the crowd");
}

/// The deterministic grid provider routes round a wall; the leaf follows the waypoints itself and
/// stops inside the radius. Two fresh runs give the same positions, bit for bit, every step.
#[test]
fn move_to_reaches_a_target_on_the_det_grid() {
    fn run() -> (u32, Vec<[u32; 3]>, Vec3) {
        let start = Vec3::new(-60.0, 0.0, 0.0);
        let target = Vec3::new(60.0, 0.0, 0.0);
        let leaf = MoveTo::new(MoveTarget::Point(target), 2.0)
            .with_provider("det_grid")
            .with_speed(25.0);
        let mut app = app_with(leaf, |app| {
            app.world_mut()
                .resource_mut::<PathProviders>()
                .insert("det_grid", DetGridProvider::new(walled_grid()));
        });
        let agent = spawn_agent(&mut app, start, Blackboard::new());
        let tree = one_leaf_tree(&app);
        app.world_mut().entity_mut(agent).insert(tree);
        let mut trace = Vec::new();
        let mut steps = 0;
        let mut status = BtStatus::Running;
        while steps < 2000 && status == BtStatus::Running {
            step(&mut app);
            steps += 1;
            let p = position(&app, agent);
            trace.push([p.x.to_bits(), p.y.to_bits(), p.z.to_bits()]);
            status = bt_status(&app, agent).unwrap_or(BtStatus::Running);
        }
        assert_eq!(
            status,
            BtStatus::Success,
            "{}",
            bb_string(&app, agent, "move_to_error")
        );
        for p in &trace {
            let x = f32::from_bits(p[0]);
            let z = f32::from_bits(p[2]);
            assert!(
                !(x.abs() < 7.0 && z.abs() < 95.0),
                "walked through the wall at ({x}, {z})"
            );
        }
        (steps, trace, position(&app, agent))
    }
    let (steps, trace, end) = run();
    assert!(
        xz_dist(end, Vec3::new(60.0, 0.0, 0.0)) <= 2.0 + 1e-3,
        "ends at {end}"
    );
    // Round the 200 m wall: well over the 120 m straight line at 25 m/s (288 steps).
    assert!(steps > 400, "took {steps} steps");
    let (steps_again, trace_again, _) = run();
    assert_eq!(steps, steps_again);
    assert_eq!(trace, trace_again, "two runs move the agent the same way");
}

/// An unreachable goal fails the leaf with a reason on the blackboard and never moves the agent:
/// disconnected navmesh islands (`unreachable`) and a goal ringed by cliffs on the grid (a partial
/// route that ends outside the radius: `partial_path`).
#[test]
fn move_to_fails_cleanly_when_unreachable() {
    // The navmesh: no route between the islands.
    let start = Vec3::new(-12.0, 0.0, 0.0);
    let leaf = MoveTo::new(MoveTarget::Point(Vec3::new(12.0, 0.0, 0.0)), 0.5).with_provider("navmesh");
    let mut app = app_with(leaf, |app| {
        app.world_mut().insert_resource(islands_mesh());
    });
    let agent = spawn_agent(&mut app, start, Blackboard::new());
    let tree = one_leaf_tree(&app);
    app.world_mut().entity_mut(agent).insert(tree);
    let (steps, status) = run_tree(&mut app, agent, 10);
    assert_eq!(status, BtStatus::Failure);
    assert_eq!(steps, 1, "fails on the planning tick");
    assert_eq!(bb_string(&app, agent, "move_to_error"), "unreachable");
    assert_eq!(bb_string(&app, agent, "move_to_state"), "failed");
    assert_eq!(
        position(&app, agent),
        start,
        "a failed plan does not move the agent"
    );

    // The grid: the partial route ends outside the ring, far from the goal.
    let start = Vec3::new(-200.0, 0.0, 0.0);
    let leaf = MoveTo::new(MoveTarget::Point(Vec3::ZERO), 2.0).with_provider("det_grid");
    let mut app = app_with(leaf, |app| {
        app.world_mut()
            .resource_mut::<PathProviders>()
            .insert("det_grid", DetGridProvider::new(ringed_grid()));
    });
    let agent = spawn_agent(&mut app, start, Blackboard::new());
    let tree = one_leaf_tree(&app);
    app.world_mut().entity_mut(agent).insert(tree);
    let (_, status) = run_tree(&mut app, agent, 10);
    assert_eq!(status, BtStatus::Failure);
    assert_eq!(bb_string(&app, agent, "move_to_error"), "partial_path");
    assert_eq!(position(&app, agent), start);

    // An unknown provider name is a failure with its own code, not a panic.
    let leaf = MoveTo::new(MoveTarget::Point(Vec3::ZERO), 2.0).with_provider("nowhere");
    let mut app = app_with(leaf, |_| {});
    let agent = spawn_agent(&mut app, start, Blackboard::new());
    let tree = one_leaf_tree(&app);
    app.world_mut().entity_mut(agent).insert(tree);
    let (_, status) = run_tree(&mut app, agent, 10);
    assert_eq!(status, BtStatus::Failure);
    assert_eq!(bb_string(&app, agent, "move_to_error"), "unknown_provider");
}

/// The target is a blackboard key; moving it more than the re-plan distance re-plans once, and the
/// agent ends at the new target, not the old one. The leaf follows the navmesh path itself here
/// (no crowd agent on the entity).
#[test]
fn move_to_replans_when_the_target_moves() {
    let first = Vec3::new(15.0, 0.0, 15.0);
    let second = Vec3::new(-15.0, 0.0, 15.0);
    let leaf = MoveTo::new(MoveTarget::Key("goal".into()), 0.5)
        .with_provider("navmesh")
        .with_speed(6.0)
        .with_replan_distance(1.0);
    let mut app = app_with(leaf, |app| {
        app.world_mut().insert_resource(floor_mesh());
    });
    let mut board = Blackboard::new();
    board.insert("goal", first);
    let agent = spawn_agent(&mut app, Vec3::new(0.0, 0.0, -15.0), board);
    let tree = one_leaf_tree(&app);
    app.world_mut().entity_mut(agent).insert(tree);
    for _ in 0..60 {
        step(&mut app);
    }
    assert_eq!(bt_status(&app, agent), Some(BtStatus::Running));
    let replans = |app: &App| match app
        .world()
        .get::<Blackboard>(agent)
        .and_then(|b| b.get("move_to_replans"))
    {
        Some(BbValue::Int(n)) => *n,
        _ => 0,
    };
    assert_eq!(replans(&app), 0);
    // A nudge inside the re-plan distance does not re-plan.
    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("board")
        .insert("goal", first + Vec3::new(0.5, 0.0, 0.0));
    step(&mut app);
    assert_eq!(replans(&app), 0);
    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("board")
        .insert("goal", second);
    let (_, status) = run_tree(&mut app, agent, 1200);
    assert_eq!(
        status,
        BtStatus::Success,
        "{}",
        bb_string(&app, agent, "move_to_error")
    );
    assert_eq!(replans(&app), 1);
    let at = position(&app, agent);
    assert!(
        xz_dist(at, second) <= 0.5 + 1e-3 && (at.y - second.y).abs() < 1.0,
        "ends at {at}"
    );
}

/// The same task in a state tree: the state's task runs the same steps as the behavior-tree leaf,
/// the agent moves the same way bit for bit, and the state's `Succeeded` transition fires.
#[test]
fn state_tree_move_to_task_matches_the_leaf() {
    const TREE: &str = r#"(
        format_version: 2,
        root: "Go",
        states: [
            (name: "Go", tasks: ["move_to_test::MoveTo"], transitions: [
                (trigger: Succeeded, target: Succeed),
                (trigger: Failed, target: Fail),
            ]),
        ],
    )"#;
    let start = Vec3::new(-60.0, 0.0, 0.0);
    let target = Vec3::new(60.0, 0.0, 30.0);
    let leaf = || {
        MoveTo::new(MoveTarget::Point(target), 2.0)
            .with_provider("det_grid")
            .with_speed(25.0)
    };
    let install = |app: &mut App| {
        app.world_mut()
            .resource_mut::<PathProviders>()
            .insert("det_grid", DetGridProvider::new(walled_grid()));
    };

    let mut bt_app = app_with(leaf(), install);
    let bt_agent = spawn_agent(&mut bt_app, start, Blackboard::new());
    let tree = one_leaf_tree(&bt_app);
    bt_app.world_mut().entity_mut(bt_agent).insert(tree);

    let mut st_app = app_with(leaf(), install);
    let st_agent = spawn_agent(&mut st_app, start, Blackboard::new());
    let def = StateTreeDef::from_ron(TREE).expect("definition");
    let tree = StateTree::compile(&def, st_app.world().resource::<BtTaskRegistry>()).expect("compiles");
    st_app.world_mut().entity_mut(st_agent).insert(tree);

    let mut steps = 0;
    loop {
        step(&mut bt_app);
        step(&mut st_app);
        steps += 1;
        let (a, b) = (position(&bt_app, bt_agent), position(&st_app, st_agent));
        assert_eq!(
            [a.x.to_bits(), a.y.to_bits(), a.z.to_bits()],
            [b.x.to_bits(), b.y.to_bits(), b.z.to_bits()],
            "step {steps}: leaf at {a}, task at {b}"
        );
        if bt_status(&bt_app, bt_agent) != Some(BtStatus::Running) {
            break;
        }
        assert!(steps < 2000, "the leaf never finished");
    }
    assert_eq!(bt_status(&bt_app, bt_agent), Some(BtStatus::Success));
    // The state tree sees its task succeed on the same step and finishes on the transition.
    step(&mut st_app);
    let st = st_app.world().get::<StateTree>(st_agent).expect("tree");
    assert_eq!(st.status(), StateTreeRunStatus::Succeeded);
    assert_eq!(bb_string(&st_app, st_agent, "move_to_state"), "arrived");
}

// --------------------------------------------------------------------------- provider seam

/// The trait is object-safe and the three providers answer through it with positions in world
/// meters; the deterministic ones give the same points on every call.
#[test]
fn providers_answer_through_the_object_safe_trait() {
    let mut world = World::new();
    world.insert_resource(floor_mesh());
    let spec = PathAgentSpec::default();
    let mut providers: Vec<(&str, Box<dyn PathProvider>)> = vec![
        ("navmesh", Box::new(NavMeshProvider)),
        ("det_grid", Box::new(DetGridProvider::new(walled_grid()))),
        ("det_navmesh", Box::new(DetNavMeshProvider::new(det_navmesh()))),
    ];
    // Clear of the grid's wall (|x| <= 7.5) and on all three fixtures.
    let (from, to) = (Vec3::new(12.0, 0.0, 2.0), Vec3::new(17.0, 0.0, 12.0));
    for (name, provider) in &mut providers {
        assert_eq!(provider.kind(), *name);
        let first = provider.plan(&world, from, to, &spec);
        let PathRequestOutcome::Ready(points) = &first else {
            panic!("{name}: {first:?}");
        };
        let end = *points.last().expect("points");
        assert!(xz_dist(end, to) < 0.75, "{name} ends at {end}");
        assert_eq!(
            first,
            provider.plan(&world, from, to, &spec),
            "{name} is repeatable"
        );
        assert!(provider.is_reachable(&world, from, to, &spec), "{name}");
    }
    // Off the navigation data.
    let far = Vec3::new(5000.0, 0.0, 5000.0);
    for (name, provider) in &mut providers {
        assert_eq!(
            provider.plan(&world, far, to, &spec),
            PathRequestOutcome::Failed(PathFailure::StartOffNav),
            "{name}"
        );
        assert!(!provider.is_reachable(&world, from, far, &spec), "{name}");
    }
    // No mesh resource: the navmesh provider says so.
    let empty = World::new();
    assert_eq!(
        NavMeshProvider.plan(&empty, from, to, &spec),
        PathRequestOutcome::Failed(PathFailure::NoNavData)
    );
    // Non-finite input.
    assert_eq!(
        providers[1].1.plan(&world, Vec3::NAN, to, &spec),
        PathRequestOutcome::Failed(PathFailure::InvalidInput)
    );
}

/// A per-tick work budget on the grid provider: the first request is served at once, later ones in
/// the same tick queue as `Pending` tickets and are served in ticket order by the next ticks.
#[test]
fn det_grid_budget_queues_requests_in_ticket_order() {
    let world = World::new();
    let spec = PathAgentSpec::default();
    let mut provider = DetGridProvider::new(walled_grid()).with_tick_budget(1);
    provider.begin_tick();
    let a = provider.plan(
        &world,
        Vec3::new(-60.0, 0.0, 0.0),
        Vec3::new(60.0, 0.0, 0.0),
        &spec,
    );
    assert!(matches!(a, PathRequestOutcome::Ready(_)), "{a:?}");
    let b = provider.plan(
        &world,
        Vec3::new(-60.0, 0.0, 10.0),
        Vec3::new(60.0, 0.0, 10.0),
        &spec,
    );
    let c = provider.plan(
        &world,
        Vec3::new(-60.0, 0.0, 20.0),
        Vec3::new(60.0, 0.0, 20.0),
        &spec,
    );
    let (PathRequestOutcome::Pending(tb), PathRequestOutcome::Pending(tc)) = (&b, &c) else {
        panic!("{b:?} {c:?}");
    };
    assert!(tb < tc);
    assert_eq!(provider.queued(), 2);
    assert_eq!(provider.poll(&world, *tb), PathRequestOutcome::Pending(*tb));
    provider.begin_tick();
    assert!(matches!(provider.poll(&world, *tb), PathRequestOutcome::Ready(_)));
    assert_eq!(
        provider.poll(&world, *tc),
        PathRequestOutcome::Pending(*tc),
        "one per tick at this budget"
    );
    provider.cancel(*tc);
    assert_eq!(provider.queued(), 0);
    assert_eq!(
        provider.poll(&world, *tc),
        PathRequestOutcome::Failed(PathFailure::UnknownTicket)
    );
    // The same request without a budget is served at once and matches the queued answer.
    let mut free = DetGridProvider::new(walled_grid());
    let direct = free.plan(
        &world,
        Vec3::new(-60.0, 0.0, 10.0),
        Vec3::new(60.0, 0.0, 10.0),
        &spec,
    );
    let mut queued = DetGridProvider::new(walled_grid()).with_tick_budget(1);
    queued.begin_tick();
    let _ = queued.plan(
        &world,
        Vec3::new(-60.0, 0.0, 0.0),
        Vec3::new(60.0, 0.0, 0.0),
        &spec,
    );
    let PathRequestOutcome::Pending(t) = queued.plan(
        &world,
        Vec3::new(-60.0, 0.0, 10.0),
        Vec3::new(60.0, 0.0, 10.0),
        &spec,
    ) else {
        panic!("queued");
    };
    queued.begin_tick();
    assert_eq!(queued.poll(&world, t), direct);
}

/// The grid's partial answers carry their reason.
#[test]
fn det_grid_partial_routes_name_their_reason() {
    let world = World::new();
    let mut provider = DetGridProvider::new(ringed_grid());
    let out = provider.plan(
        &world,
        Vec3::new(-200.0, 0.0, 0.0),
        Vec3::ZERO,
        &PathAgentSpec::default(),
    );
    let PathRequestOutcome::Partial { points, reason } = out else {
        panic!("{out:?}");
    };
    assert_eq!(reason, PartialReason::GoalUnreachable);
    let end = *points.last().expect("points");
    let r = (end.x * end.x + end.z * end.z).sqrt();
    assert!(r > 55.0, "stops outside the ring at {r} m");
    assert_eq!(reason.code(), "partial_unreachable");
    assert!(!provider.is_reachable(
        &world,
        Vec3::new(-200.0, 0.0, 0.0),
        Vec3::ZERO,
        &PathAgentSpec::default()
    ));
    // The profile picks the grid's movement class; one the settings lack is invalid input.
    let spec = PathAgentSpec {
        profile: 200,
        ..PathAgentSpec::default()
    };
    assert_eq!(
        provider.plan(
            &world,
            Vec3::new(-200.0, 0.0, 0.0),
            Vec3::new(-150.0, 0.0, 0.0),
            &spec
        ),
        PathRequestOutcome::Failed(PathFailure::InvalidInput)
    );
    assert_eq!(Profile::HOVER.0, PathAgentSpec::default().profile);
}

/// The AI plugin registers the blackboard-driven `regia_ai::MoveTo` leaf (the editors' palettes
/// list the registry) and a `navmesh` provider; the leaf reads its target, radius and provider
/// from blackboard keys.
#[test]
fn the_plugin_registers_the_palette_leaf_and_the_navmesh_provider() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    app.update();
    let registry = app.world().resource::<BtTaskRegistry>();
    assert!(
        registry.task_paths().any(|p| p == MOVE_TO_TASK),
        "palette has {MOVE_TO_TASK}"
    );
    assert!(app.world().resource::<PathProviders>().contains("navmesh"));

    app.world_mut().insert_resource(floor_mesh());
    let mut board = Blackboard::new();
    board.insert("move_target", Vec3::new(5.0, 0.0, 0.0));
    board.insert("move_acceptance_radius", 0.25f32);
    board.insert("move_provider", "navmesh".to_string());
    let agent = spawn_agent(&mut app, Vec3::new(-5.0, 0.0, 0.0), board);
    let mut builder = BehaviorTreeBuilder::new();
    let leaf = builder.leaf("go", MOVE_TO_TASK);
    let def = builder.build(leaf);
    let tree = BehaviorTree::compile(&def, app.world().resource::<BtTaskRegistry>()).expect("compiles");
    app.world_mut().entity_mut(agent).insert(tree);
    let (_, status) = run_tree(&mut app, agent, 600);
    assert_eq!(
        status,
        BtStatus::Success,
        "{}",
        bb_string(&app, agent, "move_to_error")
    );
    assert!(xz_dist(position(&app, agent), Vec3::new(5.0, 0.0, 0.0)) <= 0.25 + 1e-3);
    // No target key: a clean failure.
    app.world_mut()
        .get_mut::<Blackboard>(agent)
        .expect("board")
        .remove("move_target");
    let (_, status) = run_tree(&mut app, agent, 10);
    assert_eq!(status, BtStatus::Failure);
    assert_eq!(bb_string(&app, agent, "move_to_error"), "no_target");
}
