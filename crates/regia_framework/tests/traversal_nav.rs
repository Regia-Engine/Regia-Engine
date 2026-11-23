// SPDX-License-Identifier: MPL-2.0
//! The nav bridge (`docs/specs/traversal.md` § 7): a ladder publishes a link, a crowd agent crossing it
//! gets one request, and a character tied to the agent makes the climb.

use std::time::Duration;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState, MovementMode,
    use_rapier_character_controller,
};
use regia_framework::traversal::nav::{TraversalCrowdAgent, TraversalNavPlugin};
use regia_framework::traversal::{
    CharacterTraversal, Ladder, TraversalJumpPoint, TraversalKind, TraversalPlugin, TraversalRequest,
    TraversalSettings,
};
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, NavAuthoredLinksPlugin, NavCrowd, NavCrowdConfig, NavCrowdPlugin,
    NavLinkAnchor, NavLinkKind, NavMesh, NavMeshBuildParams, NavMeshBuilder,
};
use regia_physics::{Collider, ColliderShape, RigidBody};
use regia_scene::Transform;

const DT: f64 = 1.0 / 60.0;
const UPPER: f32 = 3.0;
const REST_Y: f32 = 1.2;

fn quad(x0: f32, x1: f32, y: f32, z0: f32, z1: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

/// A navmesh of two floors with no link: y = 0 for x < 5, y = 3 for x > 5.
fn two_floors() -> NavMesh {
    let mut tris: Vec<[Vec3; 3]> = quad(-8.0, 5.0, 0.0, -4.0, 4.0).into();
    tris.extend(quad(5.0, 15.0, UPPER, -4.0, 4.0));
    let mut params = NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    };
    params.agent.radius = 0.3;
    NavMeshBuilder::new(params)
        .build(&tris)
        .expect("the navmesh builds")
}

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(DT)));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins(regia_physics::PhysicsPlugin)
        .add_plugins(CharacterPlugin)
        .add_plugins(TraversalPlugin)
        .add_plugins(NavAuthoredLinksPlugin)
        .add_plugins(NavCrowdPlugin)
        .add_plugins(TraversalNavPlugin);
    use_rapier_character_controller(&mut app);
    app.insert_resource(two_floors());
    app.insert_resource(
        NavCrowd::new(NavCrowdConfig {
            step_seconds: 1.0 / 60.0,
            ..NavCrowdConfig::default()
        })
        .expect("a valid crowd config"),
    );
    // The floors in physics too: a block whose top is the upper floor, its face at x = 5.
    let spawn_box = |app: &mut App, half: Vec3, at: Vec3| {
        app.world_mut().spawn((
            Transform::from_translation(at),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ));
    };
    spawn_box(&mut app, Vec3::new(30.0, 0.5, 30.0), Vec3::new(0.0, -0.5, 0.0));
    spawn_box(
        &mut app,
        Vec3::new(10.0, UPPER * 0.5, 30.0),
        Vec3::new(15.0, UPPER * 0.5, 0.0),
    );
    app
}

/// A ladder on the block's face, into +X.
fn ladder(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            Transform {
                translation: Vec3::new(5.0, 0.0, 0.0),
                rotation: Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2),
                ..Default::default()
            },
            Ladder {
                height: UPPER,
                user_id: 42,
                ..Ladder::default()
            },
        ))
        .id()
}

fn add_agent(app: &mut App, at: Vec3, target: Vec3) -> CrowdAgentId {
    app.world_mut().resource_scope(|world, mut crowd: Mut<NavCrowd>| {
        let mesh = world.resource::<NavMesh>();
        let id = crowd
            .add_agent(mesh, at, CrowdAgentParams::default())
            .expect("the agent is on the mesh");
        crowd
            .set_target(mesh, id, target)
            .expect("the target is on the mesh");
        id
    })
}

fn requests(
    app: &App,
    cursor: &mut regia_ecs::ecs::message::MessageCursor<TraversalRequest>,
) -> Vec<TraversalRequest> {
    let messages = app.world().resource::<Messages<TraversalRequest>>();
    cursor.read(messages).copied().collect()
}

#[test]
fn a_ladder_publishes_its_link_and_a_crowd_agent_crossing_it_gets_one_request() {
    let mut app = app();
    let ladder = ladder(&mut app);
    app.update();
    app.update();
    let anchor = app
        .world()
        .get::<NavLinkAnchor>(ladder)
        .copied()
        .expect("an anchor");
    assert_eq!((anchor.kind, anchor.user_id), (NavLinkKind::Climb, 42));
    assert_eq!(
        app.world().resource::<NavMesh>().off_mesh_links.len(),
        1,
        "on the mesh"
    );

    let agent = add_agent(&mut app, Vec3::new(2.0, 0.0, 0.0), Vec3::new(10.0, UPPER, 0.0));
    let mut cursor = app.world().resource::<Messages<TraversalRequest>>().get_cursor();
    let mut seen = Vec::new();
    for _ in 0..1200 {
        app.update();
        seen.extend(requests(&app, &mut cursor));
    }
    assert_eq!(seen.len(), 1, "exactly one request: {seen:?}");
    let request = seen[0];
    assert_eq!(request.agent, Some(agent.0));
    assert_eq!(request.entity, None, "no character is tied to it");
    assert_eq!(request.kind, NavLinkKind::Climb);
    assert_eq!(request.user_id, 42);
    assert!(request.end.y > request.start.y + 2.0, "{request:?}");
    let crowd = app.world().resource::<NavCrowd>();
    let end = crowd.agent(agent).expect("agent").position();
    assert!(
        (end.y - UPPER).abs() < 0.2 && end.x > 5.0,
        "the agent crossed: {end}"
    );
}

#[test]
fn a_character_tied_to_the_agent_climbs_the_same_ladder() {
    let mut app = app();
    ladder(&mut app);
    app.update();
    app.update();
    let agent = add_agent(&mut app, Vec3::new(4.0, 0.0, 0.0), Vec3::new(10.0, UPPER, 0.0));
    let c = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(4.0, REST_Y, 0.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            TraversalSettings::default(),
            TraversalCrowdAgent(agent),
        ))
        .id();
    let mut cursor = app.world().resource::<Messages<TraversalRequest>>().get_cursor();
    let mut seen = Vec::new();
    let mut climbed = false;
    for _ in 0..1200 {
        app.update();
        seen.extend(requests(&app, &mut cursor));
        climbed |= app
            .world()
            .get::<CharacterTraversal>(c)
            .is_some_and(|t| t.kind == TraversalKind::Ladder);
    }
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].entity, Some(c));
    assert!(climbed, "the character got on the ladder with no input");
    let at = app.world().get::<Transform>(c).expect("transform").translation;
    assert!(
        at.x > 5.0 && (at.y - (UPPER + REST_Y)).abs() < 0.08,
        "on the upper floor: {at}"
    );
    let state = app.world().get::<CharacterState>(c).expect("state");
    assert!(state.grounded && state.mode == MovementMode::Walking, "{state:?}");
}

#[test]
fn a_jump_point_publishes_a_jump_link_and_runs_the_jump_curve() {
    let mut app = app();
    let point = app
        .world_mut()
        .spawn((
            Transform {
                translation: Vec3::new(4.5, 0.0, 2.0),
                rotation: Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2),
                ..Default::default()
            },
            TraversalJumpPoint {
                to: Vec3::new(0.0, UPPER, -1.5),
                user_id: 7,
                arc_height: 2.5,
                ..TraversalJumpPoint::default()
            },
        ))
        .id();
    app.update();
    app.update();
    let anchor = app
        .world()
        .get::<NavLinkAnchor>(point)
        .copied()
        .expect("an anchor");
    assert_eq!((anchor.kind, anchor.user_id), (NavLinkKind::Jump, 7));

    let agent = add_agent(&mut app, Vec3::new(2.0, 0.0, 2.0), Vec3::new(10.0, UPPER, 2.0));
    let c = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(4.0, REST_Y, 2.0)),
            Character,
            CharacterMovement::default(),
            CharacterInput::default(),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            TraversalSettings::default(),
            TraversalCrowdAgent(agent),
        ))
        .id();
    let mut jumped = false;
    let mut peak = 0.0f32;
    for _ in 0..1200 {
        app.update();
        let t = app
            .world()
            .get::<CharacterTraversal>(c)
            .copied()
            .unwrap_or_default();
        jumped |= t.kind == TraversalKind::Jump;
        if t.kind == TraversalKind::Jump {
            peak = peak.max(app.world().get::<Transform>(c).expect("transform").translation.y);
        }
    }
    assert!(jumped, "the jump ran");
    assert!(peak > UPPER + REST_Y + 2.0, "the authored arc: {peak}");
    let at = app.world().get::<Transform>(c).expect("transform").translation;
    assert!(
        at.x > 5.0 && (at.y - (UPPER + REST_Y)).abs() < 0.08,
        "landed up there: {at}"
    );
}

#[test]
fn an_invalid_or_disabled_source_publishes_nothing() {
    let mut app = app();
    let id = ladder(&mut app);
    app.update();
    app.update();
    assert!(app.world().get::<NavLinkAnchor>(id).is_some());
    app.world_mut().get_mut::<Ladder>(id).expect("ladder").climb_speed = f32::NAN;
    app.update();
    app.update();
    assert!(
        app.world().get::<NavLinkAnchor>(id).is_none(),
        "an invalid ladder takes its link off"
    );
    let mut ladder = app.world_mut().get_mut::<Ladder>(id).expect("ladder");
    ladder.climb_speed = 1.5;
    ladder.nav_link = false;
    app.update();
    app.update();
    assert!(app.world().get::<NavLinkAnchor>(id).is_none(), "opted out");
}

/// Spec § 7: the anchor is kept equal to its source each fixed step, so an edited ladder republishes.
#[test]
fn an_edited_ladder_republishes_its_link_at_the_next_step() {
    let mut app = app();
    let id = ladder(&mut app);
    app.update();
    app.update();
    let before = *app.world().get::<NavLinkAnchor>(id).expect("published");
    assert_eq!(before.to, Vec3::new(0.0, UPPER, -Ladder::default().exit_forward));
    {
        let mut ladder = app.world_mut().get_mut::<Ladder>(id).expect("ladder");
        ladder.height = 4.0;
        ladder.climb_speed = 2.0;
        ladder.user_id = 43;
    }
    app.update();
    app.update();
    let after = *app.world().get::<NavLinkAnchor>(id).expect("still published");
    assert_eq!(after.to, Vec3::new(0.0, 4.0, -Ladder::default().exit_forward));
    assert_eq!(after.duration, 2.0, "height / climb_speed");
    assert_eq!(after.user_id, 43);
    assert_eq!(after.kind, NavLinkKind::Climb);
    assert!(after.bidirectional);
}
