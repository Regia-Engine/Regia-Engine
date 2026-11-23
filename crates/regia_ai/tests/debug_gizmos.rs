// SPDX-License-Identifier: MPL-2.0
//! In-world AI debug draw (ADR-8463). Headless: the gizmo command buffer, no pixels.
//!
//! Its own test binary so the layers-off check can install the counting allocator.

#![cfg(feature = "debug_draw")]

use regia_ai::debug_gizmos::{
    AiDebugDraw, AiDebugFilter, BB_COLOR, BT_COLOR, CROSS, FORMATION_COLOR, GOAP_COLOR, LABEL_ANCHOR,
    LABEL_SIZE, MOVE_COLOR, SQUAD_COLOR, STACK_BLACKBOARD, STACK_BT, STACK_GOAP, STACK_ROLE, STACK_STATE,
    STATE_COLOR, label_anchor,
};
use regia_ai::{
    AiFormation, AiPlugin, AiSquad, BehaviorTree, BehaviorTreeBuilder, Blackboard, BtContext, BtStatus,
    BtTask, BtTaskRegistry, DefKind, DefNode, FormationSlot, GoapState, GoapStatus, SquadRole,
    StateTreeStatus,
};
use regia_app::prelude::*;
use regia_core::math::{Color, Vec3};
use regia_framework::character_control::MoveTo;
use regia_nav::NavAgentState;
use regia_render::gizmos::{AiDebugGizmos, GizmoBuffer, GizmoConfigGroup, GizmoConfigStore, GizmoStorage};
use regia_scene::Transform;
use regia_test_utils::alloc_counter::{self, CountingAlloc};

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

struct Hold;

impl BtTask for Hold {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Running
    }
}

struct Fail;

impl BtTask for Fail {
    fn tick(&mut self, _ctx: &mut BtContext<'_>) -> BtStatus {
        BtStatus::Failure
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    regia_app::enable_fixed_step(&mut app, 60.0);
    app.add_plugins(AiPlugin);
    // `Transform` inserts a default `GlobalTransform`. The overlay reads that world pose, so the
    // headless app has to propagate before the draw system (the same PostUpdate order as a game).
    app.add_systems(
        PostUpdate,
        regia_scene::prelude::propagate_transforms.in_set(PostUpdateSet::TransformPropagate),
    );
    {
        let mut registry = app.world_mut().resource_mut::<BtTaskRegistry>();
        registry.register_factory("test::Hold", || Box::new(Hold));
        registry.register_factory("test::Fail", || Box::new(Fail));
    }
    app.world_mut().init_resource::<GizmoConfigStore>();
    {
        let mut store = app.world_mut().resource_mut::<GizmoConfigStore>();
        store.register::<AiDebugGizmos>();
        assert!(
            store.set_enabled(AiDebugGizmos::NAME, true),
            "ai_debug group registers"
        );
    }
    app.world_mut().init_resource::<GizmoStorage<AiDebugGizmos>>();
    app
}

fn patrol_tree(registry: &BtTaskRegistry) -> BehaviorTree {
    let mut builder = BehaviorTreeBuilder::new();
    let skip = builder.add(DefNode {
        name: "skip".into(),
        kind: DefKind::Leaf,
        task: "test::Fail".into(),
        ..Default::default()
    });
    let patrol = builder.add(DefNode {
        name: "patrol".into(),
        kind: DefKind::Leaf,
        task: "test::Hold".into(),
        ..Default::default()
    });
    let root = builder.selector("root", vec![skip, patrol]);
    let def = builder.build(root);
    BehaviorTree::compile(&def, registry).expect("patrol compiles")
}

fn spawn_patrol(app: &mut App, pos: Vec3) -> Entity {
    let tree = {
        let registry = app.world().resource::<BtTaskRegistry>();
        patrol_tree(registry)
    };
    app.world_mut()
        .spawn((tree, Transform::from_translation(pos), Blackboard::new()))
        .id()
}

fn tick_and_draw(app: &mut App) {
    app.world_mut().run_schedule(FixedUpdate);
    app.update();
}

fn strips(app: &App) -> Vec<Vec3> {
    app.world()
        .resource::<GizmoStorage<AiDebugGizmos>>()
        .view()
        .strip_positions
        .to_vec()
}

fn lines(app: &App) -> Vec<Vec3> {
    app.world()
        .resource::<GizmoStorage<AiDebugGizmos>>()
        .view()
        .list_positions
        .to_vec()
}

fn same_points(left: &[Vec3], right: &[Vec3]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| (left.is_nan() && right.is_nan()) || left == right)
}

fn contains_strips(haystack: &[Vec3], needle: &[Vec3]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| same_points(window, needle))
}

fn text_strips(pos: Vec3, stack: u32, text: &str, color: Color) -> Vec<Vec3> {
    let mut buffer = GizmoBuffer::<AiDebugGizmos>::new();
    buffer.text(label_anchor(pos, stack), text, LABEL_SIZE, LABEL_ANCHOR, color);
    buffer.strip_positions
}

fn section_strips(pos: Vec3, stack: u32, sections: &[(&str, Color)]) -> Vec<Vec3> {
    let mut buffer = GizmoBuffer::<AiDebugGizmos>::new();
    buffer.text_sections(label_anchor(pos, stack), sections, LABEL_SIZE, LABEL_ANCHOR);
    buffer.strip_positions
}

fn has_segment(list: &[Vec3], start: Vec3, end: Vec3) -> bool {
    let (pairs, rest) = list.as_chunks::<2>();
    assert!(rest.is_empty(), "line list is not paired");
    pairs.iter().any(|pair| pair[0] == start && pair[1] == end)
}

fn enable(app: &mut App, layers: impl FnOnce(&mut AiDebugDraw)) {
    let mut draw = app.world_mut().resource_mut::<AiDebugDraw>();
    layers(&mut draw);
}

/// The BT layer writes the running leaf's name above the agent, not the selector or the failed leaf.
#[test]
fn bt_layer_labels_the_active_leaf() {
    let mut app = app();
    let pos = Vec3::new(1.0, 0.0, 2.0);
    spawn_patrol(&mut app, pos);
    enable(&mut app, |draw| draw.layers.behavior_tree = true);
    tick_and_draw(&mut app);
    let drawn = strips(&app);
    let patrol = text_strips(pos, STACK_BT, "patrol", BT_COLOR);
    assert!(
        same_points(&drawn, &patrol),
        "expected the patrol leaf stroke, got {} strip verts",
        drawn.len()
    );
    assert!(lines(&app).is_empty(), "the BT layer is text only");
    assert_eq!(app.world().resource::<AiDebugDraw>().dropped, 0);
}

/// State-tree, GOAP and squad layers write their crosses, the plan step, and the leader line.
#[test]
fn state_tree_goap_squad_layers_emit_their_shapes() {
    let mut app = app();
    let state_pos = Vec3::new(0.0, 3.0, 0.0);
    let goap_pos = Vec3::new(8.0, 0.0, 0.0);
    let leader_pos = Vec3::new(20.0, 0.0, 0.0);
    let member_pos = Vec3::new(24.0, 0.0, 0.0);
    app.world_mut().spawn((
        Transform::from_translation(state_pos),
        StateTreeStatus {
            active_path: vec!["Idle".into(), "Patrol".into()],
            ..StateTreeStatus::default()
        },
    ));
    app.world_mut().spawn((
        Transform::from_translation(goap_pos),
        GoapStatus {
            state: GoapState::Executing,
            steps: vec!["pickup".into()],
            next_step: 0,
            ..GoapStatus::default()
        },
    ));
    let leader = app
        .world_mut()
        .spawn(Transform::from_translation(leader_pos))
        .id();
    let member = app
        .world_mut()
        .spawn(Transform::from_translation(member_pos))
        .id();
    let mut role = SquadRole::new("Scout", 1);
    role.auto_fill = false;
    role.holders = vec![member];
    app.world_mut().spawn((
        AiSquad {
            name: "alpha".into(),
            members: vec![leader, member],
            leader: Some(leader),
            roles: vec![role],
            elections: 0,
        },
        Transform::from_translation(Vec3::new(20.0, 1.0, 0.0)),
    ));
    enable(&mut app, |draw| {
        draw.layers.state_tree = true;
        draw.layers.goap = true;
        draw.layers.squad = true;
    });
    tick_and_draw(&mut app);
    let list = lines(&app);
    let state_x0 = state_pos + Vec3::X * CROSS;
    let state_x1 = state_pos - Vec3::X * CROSS;
    let state_z0 = state_pos + Vec3::Z * CROSS;
    let state_z1 = state_pos - Vec3::Z * CROSS;
    assert!(has_segment(&list, state_x0, state_x1), "state-tree cross X");
    assert!(has_segment(&list, state_z0, state_z1), "state-tree cross Z");
    assert!(
        has_segment(&list, goap_pos + Vec3::X * CROSS, goap_pos - Vec3::X * CROSS),
        "GOAP cross"
    );
    assert!(has_segment(&list, leader_pos, member_pos), "squad leader line");
    let drawn = strips(&app);
    assert!(
        contains_strips(
            &drawn,
            &text_strips(state_pos, STACK_STATE, "Patrol", STATE_COLOR)
        ),
        "state name"
    );
    assert!(
        contains_strips(&drawn, &text_strips(goap_pos, STACK_GOAP, "pickup", GOAP_COLOR)),
        "GOAP step"
    );
    assert!(
        contains_strips(&drawn, &text_strips(member_pos, STACK_ROLE, "S", SQUAD_COLOR)),
        "role letter"
    );
}

/// Formation slots and the MoveTo / remaining-nav path are shapes on their own layers.
#[test]
fn formation_slots_and_move_to_draw_their_shapes() {
    let mut app = app();
    let origin = Vec3::ZERO;
    let corner = Vec3::new(2.0, 0.0, 1.0);
    let target = Vec3::new(4.0, 0.0, 1.0);
    app.world_mut().spawn((
        Transform::from_translation(origin),
        MoveTo::new(target),
        NavAgentState {
            waypoints: vec![origin, corner, target],
            next_waypoint: 1,
            target: Some(target),
            ..NavAgentState::default()
        },
    ));
    let member_pos = Vec3::new(3.0, 0.0, 4.0);
    let slot_pos = Vec3::new(3.0, 0.0, 1.0);
    let member = app
        .world_mut()
        .spawn(Transform::from_translation(member_pos))
        .id();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
        AiFormation {
            slots: vec![FormationSlot {
                index: 0,
                position: slot_pos,
                member,
            }],
            ..AiFormation::default()
        },
    ));
    enable(&mut app, |draw| {
        draw.layers.formation = true;
        draw.layers.move_to = true;
    });
    tick_and_draw(&mut app);
    let list = lines(&app);
    assert!(has_segment(&list, origin, target), "MoveTo line");
    assert!(
        has_segment(&list, target + Vec3::X * CROSS, target - Vec3::X * CROSS),
        "target cross"
    );
    assert!(
        has_segment(&list, slot_pos + Vec3::X * CROSS, slot_pos - Vec3::X * CROSS),
        "slot cross"
    );
    assert!(has_segment(&list, slot_pos, member_pos), "slot to member");
    let drawn = strips(&app);
    assert!(
        contains_strips(&drawn, &[corner, target, Vec3::NAN]),
        "remaining waypoints"
    );
    let _ = FORMATION_COLOR;
    let _ = MOVE_COLOR;
}

/// One label is kept; the rest increment the dropped counter. A cap of zero drops every label.
#[test]
fn label_cap_drops_and_counts() {
    let mut app = app();
    let positions = [Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0)];
    for pos in positions {
        spawn_patrol(&mut app, pos);
    }
    enable(&mut app, |draw| {
        draw.layers.behavior_tree = true;
        draw.label_cap = 1;
    });
    tick_and_draw(&mut app);
    assert_eq!(app.world().resource::<AiDebugDraw>().dropped, 2);
    let drawn = strips(&app);
    let matches = positions
        .into_iter()
        .filter(|pos| same_points(&drawn, &text_strips(*pos, STACK_BT, "patrol", BT_COLOR)))
        .count();
    assert_eq!(matches, 1, "exactly one patrol label");

    enable(&mut app, |draw| draw.label_cap = 0);
    app.world_mut()
        .resource_mut::<GizmoStorage<AiDebugGizmos>>()
        .clear();
    tick_and_draw(&mut app);
    assert_eq!(app.world().resource::<AiDebugDraw>().dropped, 3);
    assert!(strips(&app).is_empty(), "a zero cap draws no text");
    assert!(lines(&app).is_empty(), "the BT layer has no shapes");
}

/// Layers off, and the group off, queue nothing. The early-out itself does not allocate.
#[test]
fn layers_off_emit_nothing() {
    let mut draw = AiDebugDraw {
        dropped: 4,
        ..AiDebugDraw::default()
    };
    let (running, counted) = alloc_counter::measure(|| draw.begin_frame());
    assert!(!running, "default layers are off");
    assert_eq!(counted.allocs, 0, "begin_frame allocates");
    assert_eq!(draw.dropped, 0);

    let mut app = app();
    spawn_patrol(&mut app, Vec3::new(1.0, 2.0, 3.0));
    tick_and_draw(&mut app);
    assert!(
        app.world().resource::<GizmoStorage<AiDebugGizmos>>().is_empty(),
        "layers off"
    );
    assert_eq!(app.world().resource::<AiDebugDraw>().dropped, 0);

    enable(&mut app, |draw| draw.layers.behavior_tree = true);
    app.world_mut()
        .resource_mut::<GizmoConfigStore>()
        .set_enabled(AiDebugGizmos::NAME, false);
    tick_and_draw(&mut app);
    assert!(
        app.world().resource::<GizmoStorage<AiDebugGizmos>>().is_empty(),
        "group off"
    );
}

/// Selected keeps one agent. Radius keeps agents inside the distance from that agent.
#[test]
fn selected_and_radius_filters_keep_the_other_agents_out() {
    let mut app = app();
    let near = Vec3::new(1.0, 0.0, 0.0);
    let far = Vec3::new(30.0, 0.0, 0.0);
    let selected = spawn_patrol(&mut app, near);
    spawn_patrol(&mut app, far);
    enable(&mut app, |draw| {
        draw.layers.behavior_tree = true;
        draw.filter = AiDebugFilter::Selected;
        draw.selected = Some(selected);
    });
    tick_and_draw(&mut app);
    assert!(
        same_points(&strips(&app), &text_strips(near, STACK_BT, "patrol", BT_COLOR)),
        "selected agent only"
    );

    app.world_mut()
        .resource_mut::<GizmoStorage<AiDebugGizmos>>()
        .clear();
    enable(&mut app, |draw| {
        draw.filter = AiDebugFilter::Radius;
        draw.radius = 5.0;
    });
    tick_and_draw(&mut app);
    assert!(
        same_points(&strips(&app), &text_strips(near, STACK_BT, "patrol", BT_COLOR)),
        "radius keeps the near agent"
    );
}

/// A watched blackboard key is labeled; a missing key is not.
#[test]
fn blackboard_watch_key_labels_the_value() {
    let mut app = app();
    let pos = Vec3::new(0.0, 4.0, 0.0);
    let mut board = Blackboard::new();
    board.insert("alert", true);
    app.world_mut().spawn((Transform::from_translation(pos), board));
    enable(&mut app, |draw| {
        draw.layers.blackboard = true;
        draw.watch_keys = vec!["alert".into(), "missing".into()];
    });
    tick_and_draw(&mut app);
    let reference = section_strips(
        pos,
        STACK_BLACKBOARD,
        &[("alert", BB_COLOR), ("=", BB_COLOR), ("true", BB_COLOR)],
    );
    assert!(same_points(&strips(&app), &reference), "one blackboard label");
    assert_eq!(app.world().resource::<AiDebugDraw>().dropped, 0);
}
