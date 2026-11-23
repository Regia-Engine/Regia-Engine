// SPDX-License-Identifier: MPL-2.0
//! Navigation's side of the floating origin (`docs/specs/78-world-streaming.md` § 12.2). When a
//! streamed world moves its origin by whole cells, every navigation store that holds world positions
//! outside `Transform` moves with it in the same fixed step: the [`NavMesh`], the [`NavTileCache`]
//! (which rebuilds its tiles), [`NavObstacles`], [`NavCrowd`] agents, [`NavLaneGraph`] lanes, queued
//! [`NavRegionQueue`] boxes and each [`NavAgentState`]'s waypoints.
//!
//! Every nav plugin adds [`NavOriginPlugin`] once. It does nothing until a rebase happens, so a level
//! without streaming pays one empty cursor read per fixed step.
//!
//! The shift is applied by whichever nav system runs first in the rebase's fixed step — the
//! standalone `shift_nav_origin` system or the first nav step system — through one shared
//! [`MessageCursor`], so every store and every [`NavAgentState`] (including one the step will
//! not steer) moves exactly once and no nav step ever steers with pre-shift data, without an
//! ordering edge from the shift system to the nav steps (the edge would close a cycle with games
//! that order a nav step before the streaming sets; audit sprint-audit-g-nav-determinism H2).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_ecs::ecs::message::MessageCursor;
use regia_scene::streaming::{OriginShifted, StreamSet};

use crate::{NavAgentState, NavCrowd, NavLaneGraph, NavMesh, NavObstacles, NavRegionQueue, NavTileCache};

/// Moves navigation with the floating origin. Added once by whichever nav plugin comes first.
#[derive(Default)]
pub struct NavOriginPlugin;

/// The one read cursor every nav shift applier shares: each `OriginShifted` message is consumed
/// exactly once, by whichever system runs first in the fixed step.
#[derive(Resource, Default)]
pub(crate) struct NavOriginShiftCursor(pub(crate) MessageCursor<OriginShifted>);

impl Plugin for NavOriginPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<OriginShifted>()
            .init_resource::<NavOriginShiftCursor>()
            .add_systems(
                FixedUpdate,
                // Not ordered against the nav steps: a game may order its crowd steering before
                // physics, which runs before the streaming sets, and an edge here would close a
                // cycle. The step systems drain the same cursor at the start of the step, so the
                // first nav system of the tick applies the shift whatever the executor picks.
                shift_nav_origin.after(StreamSet::Rebase).before(StreamSet::Plan),
            );
    }
}

/// Adds [`NavOriginPlugin`] unless the app has it.
pub(crate) fn ensure(app: &mut App) {
    if !app.has_plugin(std::any::type_name::<NavOriginPlugin>()) {
        app.add_plugins(NavOriginPlugin);
    }
}

/// The nav stores a rebase moves — each present only when its plugin or the game inserted it —
/// as one system parameter, so the shift system and the step systems take a single argument.
#[derive(regia_ecs::ecs::system::SystemParam)]
pub(crate) struct NavStores<'w> {
    /// The baked navigation mesh, when a game inserted one.
    pub(crate) mesh: Option<ResMut<'w, NavMesh>>,
    /// The streaming tile cache, when a game inserted one.
    pub(crate) cache: Option<ResMut<'w, NavTileCache>>,
    /// The obstacle volumes, when a game inserted them.
    pub(crate) obstacles: Option<ResMut<'w, NavObstacles>>,
    /// The crowd, when a game inserted one.
    pub(crate) crowd: Option<ResMut<'w, NavCrowd>>,
    /// The lane graph, when a game inserted one.
    pub(crate) lanes: Option<ResMut<'w, NavLaneGraph>>,
    /// The region rebuild queue, when a game inserted one.
    pub(crate) regions: Option<ResMut<'w, NavRegionQueue>>,
}

/// The shared origin-shift cursor and its messages as one system parameter. Systems that drain
/// the shared cursor take this instead of the two resources (keeping them under the system
/// argument limit); [`NavOriginShiftParams::apply`] shifts the stores and every ECS agent state.
#[derive(regia_ecs::ecs::system::SystemParam)]
pub(crate) struct NavOriginShiftParams<'w> {
    cursor: Option<ResMut<'w, NavOriginShiftCursor>>,
    messages: Option<Res<'w, Messages<OriginShifted>>>,
}

impl NavOriginShiftParams<'_> {
    /// Drains any un-consumed `OriginShifted` messages and moves the nav stores and every ECS
    /// agent state by them. A no-op when nothing is pending: the first nav system of the tick
    /// applies the shift through the shared cursor and everyone else reads an empty cursor.
    pub(crate) fn apply(&mut self, stores: &mut NavStores, agents: &mut Query<&mut NavAgentState>) {
        if let (Some(cursor), Some(messages)) = (self.cursor.as_deref_mut(), self.messages.as_deref()) {
            let deltas = apply_origin_shifts(&mut cursor.0, messages, stores);
            for delta in &deltas {
                for mut state in agents.iter_mut() {
                    state.shift_origin(*delta);
                }
            }
        }
    }
}

/// Drains the un-consumed `OriginShifted` messages through the shared `cursor` and moves every
/// store by `-shift`, returning the deltas applied (empty when the rebase was already applied by
/// another nav system this tick). The caller passes those deltas to [`shift_agent_states`]: the
/// cursor is consumed here, so a later system will not move the waypoints. Called first thing by
/// the standalone shift system and by every nav step system, so the nav stores shift before any
/// nav step of the tick and exactly once.
pub(crate) fn apply_origin_shifts(
    cursor: &mut MessageCursor<OriginShifted>,
    messages: &Messages<OriginShifted>,
    stores: &mut NavStores,
) -> Vec<Vec3> {
    let deltas: Vec<Vec3> = cursor
        .read(messages)
        .map(|message| -Vec3::from_array(message.rebase.shift))
        .collect();
    if deltas.is_empty() {
        return deltas;
    }
    let _span = tracing::info_span!("nav_shift_origin", shifts = deltas.len()).entered();
    for delta in &deltas {
        if let Some(mesh) = stores.mesh.as_deref_mut() {
            mesh.shift_origin(*delta);
        }
        if let Some(cache) = stores.cache.as_deref_mut() {
            cache.shift_origin(*delta);
        }
        if let Some(obstacles) = stores.obstacles.as_deref_mut() {
            obstacles.shift_origin(*delta);
        }
        if let Some(crowd) = stores.crowd.as_deref_mut() {
            crowd.shift_origin(*delta);
        }
        if let Some(lanes) = stores.lanes.as_deref_mut() {
            lanes.shift_origin(*delta);
        }
        if let Some(regions) = stores.regions.as_deref_mut() {
            regions.shift_origin(*delta);
        }
    }
    deltas
}

/// Moves every [`NavAgentState`] target and waypoint by each delta. The caller is the nav system
/// that consumed the rebase this tick, so parked agents are included: the steering query skips
/// them, and a later shift system then sees an empty cursor.
pub(crate) fn shift_agent_states(deltas: &[Vec3], agents: &mut Query<&mut NavAgentState>) {
    for delta in deltas {
        for mut state in &mut *agents {
            state.shift_origin(*delta);
        }
    }
}

/// `FixedUpdate`, right after `StreamSet::Rebase`: shifts every store and agent state by
/// `-shift` (unless a nav step system already did, through the shared cursor).
pub(crate) fn shift_nav_origin(
    mut cursor: ResMut<NavOriginShiftCursor>,
    messages: Res<Messages<OriginShifted>>,
    mut stores: NavStores,
    mut agents: Query<&mut NavAgentState>,
) {
    let deltas = apply_origin_shifts(&mut cursor.0, &messages, &mut stores);
    shift_agent_states(&deltas, &mut agents);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NavAgent, NavMeshBuildParams, NavMeshBuilder, NavPlugin, NavQuery};
    use regia_ecs::ecs::system::RunSystemOnce as _;
    use regia_scene::Transform;
    use regia_scene::floating_origin::Rebase;

    /// A 20 m floor centered at `center`, 0.5 m cells, a zero-radius agent.
    fn floor(center: Vec3) -> NavMesh {
        let h = 10.0;
        let c = |x: f32, z: f32| center + Vec3::new(x, 0.0, z);
        let tris = vec![[c(-h, -h), c(h, -h), c(h, h)], [c(-h, -h), c(h, h), c(-h, h)]];
        let mut params = NavMeshBuildParams {
            cell_size: 0.5,
            ..Default::default()
        };
        params.agent.radius = 0.0;
        NavMeshBuilder::new(params).build(&tris).expect("builds")
    }

    /// A path around a box in the middle of the floor.
    fn detour(mesh: &NavMesh, center: Vec3) -> Vec<Vec3> {
        let mut obstacles = NavObstacles::default();
        obstacles.add_box(
            center + Vec3::new(-1.0, -1.0, -4.0),
            center + Vec3::new(1.0, 1.0, 4.0),
        );
        let query = NavQuery {
            mesh,
            obstacles: Some(&obstacles),
        };
        query
            .find_path(
                center + Vec3::new(-6.0, 0.0, 0.5),
                center + Vec3::new(6.0, 0.0, 0.5),
                Vec3::splat(2.0),
            )
            .expect("path")
            .waypoints
    }

    fn close(a: &[Vec3], b: &[Vec3]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(p, q)| p.distance(*q) < 1.0e-3)
    }

    /// Spec 78 § 12.2: a mesh shifted by whole cells plans the same path as one built in place.
    #[test]
    fn a_shifted_mesh_plans_the_path_a_mesh_built_there_plans() {
        let before = Vec3::new(4100.0, 3.0, 3080.0);
        let delta = Vec3::new(-4096.0, 0.0, -3072.0);
        let mut mesh = floor(before);
        let path_before = detour(&mesh, before);
        assert!(path_before.len() > 2, "the box bends the path: {path_before:?}");
        mesh.shift_origin(delta);
        let shifted = detour(&mesh, before + delta);
        let moved: Vec<Vec3> = path_before.iter().map(|p| *p + delta).collect();
        assert!(close(&shifted, &moved), "{shifted:?} vs {moved:?}");
        let built_there = floor(before + delta);
        assert!(close(&detour(&built_there, before + delta), &shifted));
        assert!(mesh.min.distance(built_there.min) < 1.0e-3);
        assert!(mesh.max.distance(built_there.max) < 1.0e-3);
    }

    /// H2 (audit sprint-audit-g-nav-determinism): a nav step that lands between the runtime's
    /// rebase (which moves entity `Transform`s and writes `OriginShifted`) and the nav shift
    /// (which moves `NavAgentState`'s waypoints) steers one step with the waypoints still in the
    /// old frame. The step systems drain the same `OriginShifted` cursor, so the first nav system
    /// of the tick applies the shift and the executor's order cannot change the result: with the
    /// shift system forced before the step and forced after it, both apps must match a manual
    /// "shift, then one step".
    #[test]
    fn a_rebase_applies_before_the_agent_step_whatever_the_system_order() {
        use crate::NavStep;
        use crate::agent::step_nav_agents;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let mesh = floor(Vec3::new(2000.0, 0.0, 0.0));
        // The path runs mostly along +Z so the shift (along X) is not collinear with it: a step
        // with stale waypoints would steer a visibly different direction.
        let start = Vec3::new(1996.0, 0.0, 1998.0);
        let goal = Vec3::new(1998.0, 0.0, 2004.0);

        let spawn_agent = |world: &mut World| {
            let state = NavAgentState {
                target: Some(goal),
                waypoints: vec![goal],
                ..NavAgentState::default()
            };
            world
                .spawn((NavAgent::default(), state, Transform::from_translation(start)))
                .id()
        };
        let outcome = |world: &World, agent: Entity| {
            (
                world.get::<Transform>(agent).expect("t").translation,
                world.get::<NavAgentState>(agent).expect("state").velocity,
            )
        };

        // Reference: move the transform and the waypoints by hand, then one explicit step.
        let mut reference = World::new();
        reference.insert_resource(FixedTime::default());
        reference.insert_resource(mesh.clone());
        let agent = spawn_agent(&mut reference);
        reference.get_mut::<Transform>(agent).expect("t").translation += delta;
        reference
            .get_mut::<NavAgentState>(agent)
            .expect("state")
            .shift_origin(delta);
        reference
            .run_system_once(step_nav_agents)
            .expect("the system runs");
        let reference = outcome(&reference, agent);

        // The two forced orders. The runtime rebase always precedes the nav shift (spec 78 § 4.3);
        // the test forces the nav shift on either side of the agent step.
        let run = |shift_after_step: bool| {
            // The streaming runtime's side of the rebase: move the entity with the world and
            // publish the message (what `StreamSet::Rebase` does for cell members).
            let runtime_rebase =
                move |mut agents: Query<&mut Transform>, mut shifted: MessageWriter<OriginShifted>| {
                    for mut t in &mut agents {
                        t.translation += delta;
                    }
                    shifted.write(OriginShifted {
                        rebase: Rebase {
                            cell_delta: [1, 0, 0],
                            shift,
                        },
                        origin_cell: [1, 0, 0],
                    });
                };
            let mut app = App::new();
            app.add_message::<OriginShifted>()
                .init_resource::<NavOriginShiftCursor>()
                .insert_resource(FixedTime::default())
                .insert_resource(mesh.clone());
            let agent = spawn_agent(app.world_mut());
            if shift_after_step {
                app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_agents));
                app.add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
                app.add_systems(FixedUpdate, shift_nav_origin.after(NavStep));
            } else {
                app.add_systems(FixedUpdate, runtime_rebase.before(shift_nav_origin));
                app.add_systems(FixedUpdate, shift_nav_origin.before(NavStep));
                app.add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
            }
            app.world_mut().run_schedule(FixedUpdate);
            outcome(app.world(), agent)
        };
        assert_eq!(run(false), reference, "shift before the step: shift-then-step");
        assert_eq!(
            run(true),
            reference,
            "a step before the shift still steers with the new frame"
        );
    }

    /// The crowd step and the lane step both drain the shared cursor. A rebase they see first
    /// must still move every `NavAgentState`, including one the agent step will not steer.
    #[test]
    fn a_crowd_or_lane_step_that_sees_the_rebase_first_still_shifts_agent_waypoints() {
        use crate::NavLinkTraversal;
        use crate::agent::step_nav_agents;
        use crate::crowd::step_nav_crowd;
        use crate::lanes::step_nav_lanes;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let goal = Vec3::new(1998.0, 0.0, 2004.0);
        let start = Vec3::new(1996.0, 0.0, 1998.0);

        let run = |lane_first: bool| {
            let mut app = App::new();
            app.add_message::<OriginShifted>()
                .add_message::<NavLinkTraversal>()
                .init_resource::<NavOriginShiftCursor>()
                .insert_resource(FixedTime::default())
                .insert_resource(floor(Vec3::new(2000.0, 0.0, 0.0)));
            let agent = app
                .world_mut()
                .spawn((
                    NavAgent::default(),
                    NavAgentState {
                        target: Some(goal),
                        waypoints: vec![goal],
                        ..NavAgentState::default()
                    },
                    Transform::from_translation(start),
                ))
                .id();
            let runtime_rebase =
                move |mut agents: Query<&mut Transform>, mut shifted: MessageWriter<OriginShifted>| {
                    for mut t in &mut agents {
                        t.translation += delta;
                    }
                    shifted.write(OriginShifted {
                        rebase: Rebase {
                            cell_delta: [1, 0, 0],
                            shift,
                        },
                        origin_cell: [1, 0, 0],
                    });
                };
            if lane_first {
                app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_lanes));
                app.add_systems(FixedUpdate, step_nav_lanes.before(shift_nav_origin));
            } else {
                app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_crowd));
                app.add_systems(FixedUpdate, step_nav_crowd.before(shift_nav_origin));
            }
            app.add_systems(FixedUpdate, shift_nav_origin);
            // The agent step runs after the dedicated shift. The crowd or lane step already
            // consumed the cursor, so this later step is not what moves the waypoints.
            app.add_systems(FixedUpdate, step_nav_agents.after(shift_nav_origin));
            app.world_mut().run_schedule(FixedUpdate);
            app.world().get::<NavAgentState>(agent).expect("state").waypoints[0]
        };

        assert_eq!(
            run(false),
            goal + delta,
            "the crowd step consumed the rebase and left the waypoints behind"
        );
        assert_eq!(
            run(true),
            goal + delta,
            "the lane step consumed the rebase and left the waypoints behind"
        );
    }

    /// Parked agents are not steered, but their waypoints are still world positions. When the
    /// agent step is the system that consumes the rebase, it has to move them too: the dedicated
    /// shift system then sees an empty cursor.
    #[test]
    fn a_parked_agents_waypoints_move_when_the_agent_step_consumes_the_rebase() {
        use crate::NavStep;
        use crate::agent::step_nav_agents;
        use regia_scene::components::Visibility;
        use regia_scene::layers::LayerParked;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let goal = Vec3::new(1998.0, 0.0, 2004.0);
        let start = Vec3::new(1996.0, 0.0, 1998.0);
        let mut app = App::new();
        app.add_message::<OriginShifted>()
            .init_resource::<NavOriginShiftCursor>()
            .insert_resource(FixedTime::default())
            .insert_resource(floor(Vec3::new(2000.0, 0.0, 0.0)));
        let parked = app
            .world_mut()
            .spawn((
                NavAgent::default(),
                NavAgentState {
                    target: Some(goal),
                    waypoints: vec![goal],
                    ..NavAgentState::default()
                },
                Transform::from_translation(start),
                LayerParked {
                    restore: Visibility::Inherited,
                    inherited: false,
                },
            ))
            .id();
        let runtime_rebase = move |mut agents: Query<&mut Transform>,
                                   mut shifted: MessageWriter<OriginShifted>| {
            for mut t in &mut agents {
                t.translation += delta;
            }
            shifted.write(OriginShifted {
                rebase: Rebase {
                    cell_delta: [1, 0, 0],
                    shift,
                },
                origin_cell: [1, 0, 0],
            });
        };
        app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_agents));
        app.add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
        app.add_systems(FixedUpdate, shift_nav_origin.after(NavStep));
        app.world_mut().run_schedule(FixedUpdate);
        let waypoints = &app.world().get::<NavAgentState>(parked).expect("state").waypoints;
        assert_eq!(
            waypoints,
            &vec![goal + delta],
            "a parked agent's plan stayed in the pre-rebase frame"
        );
    }

    /// The plugin shifts every store and agent in the fixed step of the rebase.
    #[test]
    fn a_rebase_moves_the_mesh_obstacles_and_agent_waypoints() {
        let mut app = App::new();
        app.add_plugins(NavPlugin);
        app.insert_resource(FixedTime::default());
        // Another nav plugin asking for the origin hook adds no second shift.
        ensure(&mut app);
        let center = Vec3::new(2000.0, 0.0, 0.0);
        app.insert_resource(floor(center));
        let mut obstacles = NavObstacles::default();
        obstacles.add_box(center - Vec3::ONE, center + Vec3::ONE);
        app.insert_resource(obstacles);
        let agent = app
            .world_mut()
            .spawn((
                NavAgent::default(),
                NavAgentState {
                    target: Some(center + Vec3::X),
                    waypoints: vec![center, center + Vec3::X],
                    ..NavAgentState::default()
                },
                regia_scene::Transform::from_translation(center),
            ))
            .id();
        let shift = [1024.0, 0.0, 0.0];
        app.world_mut().write_message(OriginShifted {
            rebase: Rebase {
                cell_delta: [1, 0, 0],
                shift,
            },
            origin_cell: [1, 0, 0],
        });
        app.world_mut().run_schedule(FixedUpdate);
        let moved = center - Vec3::from_array(shift);
        let mesh = app.world().resource::<NavMesh>();
        assert!(mesh.min.distance(floor(moved).min) < 1.0e-3, "{:?}", mesh.min);
        let boxes: Vec<_> = app.world().resource::<NavObstacles>().boxes().collect();
        assert_eq!(boxes, vec![(moved - Vec3::ONE, moved + Vec3::ONE)]);
        let state = app.world().get::<NavAgentState>(agent).expect("state");
        assert_eq!(state.target, Some(moved + Vec3::X));
        assert_eq!(state.waypoints, vec![moved, moved + Vec3::X]);
        // Read once: the next step does not shift again.
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().get::<NavAgentState>(agent).expect("state").target,
            Some(moved + Vec3::X)
        );
    }

    /// H2 regression (audit kimirev-fix-nav-determinism, finding 1): a crowd step that runs
    /// before the agent step in the rebase tick drains the shared cursor; it must then also shift
    /// the ECS agents' `NavAgentState`s, or an app with `NavPlugin` + `NavCrowdPlugin` strands the
    /// agents' waypoints a whole cell off forever.
    #[test]
    fn a_crowd_step_first_still_shifts_the_agent_waypoints() {
        use crate::NavStep;
        use crate::agent::step_nav_agents;
        use crate::crowd::step_nav_crowd;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let center = Vec3::new(2000.0, 0.0, 0.0);
        let start = center + Vec3::new(-4.0, 0.0, 0.0);
        let goal = center + Vec3::new(4.0, 0.0, 0.0);

        let mut app = App::new();
        app.add_message::<OriginShifted>()
            .add_message::<crate::NavLinkTraversal>()
            .init_resource::<NavOriginShiftCursor>()
            .insert_resource(FixedTime::default())
            .insert_resource(floor(center));
        let agent = app
            .world_mut()
            .spawn((
                NavAgent::default(),
                NavAgentState {
                    target: Some(goal),
                    waypoints: vec![goal],
                    ..NavAgentState::default()
                },
                Transform::from_translation(start),
            ))
            .id();
        // The streaming runtime's side of the rebase, then the crowd step forced ahead of the
        // agent step — the order that used to strand the ECS agent's waypoints.
        let runtime_rebase = move |mut transforms: Query<&mut Transform>,
                                   mut shifted: MessageWriter<OriginShifted>| {
            for mut t in &mut transforms {
                t.translation += delta;
            }
            shifted.write(OriginShifted {
                rebase: Rebase {
                    cell_delta: [1, 0, 0],
                    shift,
                },
                origin_cell: [1, 0, 0],
            });
        };
        app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_crowd));
        app.add_systems(FixedUpdate, step_nav_crowd.before(step_nav_agents));
        app.add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
        app.world_mut().run_schedule(FixedUpdate);

        let state = app.world().get::<NavAgentState>(agent).expect("state");
        assert_eq!(state.target, Some(goal + delta));
        assert_eq!(state.waypoints, vec![goal + delta]);
    }

    /// H2 regression (audit kimirev-fix-nav-determinism, finding 1): the lane step drains the
    /// same cursor and must shift the ECS agents' `NavAgentState`s too, for the same reason as
    /// the crowd step in `a_crowd_step_first_still_shifts_the_agent_waypoints`.
    #[test]
    fn a_lane_step_first_still_shifts_the_agent_waypoints() {
        use crate::NavStep;
        use crate::agent::step_nav_agents;
        use crate::lanes::step_nav_lanes;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let center = Vec3::new(2000.0, 0.0, 0.0);
        let start = center + Vec3::new(-4.0, 0.0, 0.0);
        let goal = center + Vec3::new(4.0, 0.0, 0.0);

        let mut app = App::new();
        app.add_message::<OriginShifted>()
            .init_resource::<NavOriginShiftCursor>()
            .insert_resource(FixedTime::default())
            .insert_resource(floor(center));
        let agent = app
            .world_mut()
            .spawn((
                NavAgent::default(),
                NavAgentState {
                    target: Some(goal),
                    waypoints: vec![goal],
                    ..NavAgentState::default()
                },
                Transform::from_translation(start),
            ))
            .id();
        let runtime_rebase = move |mut transforms: Query<&mut Transform>,
                                   mut shifted: MessageWriter<OriginShifted>| {
            for mut t in &mut transforms {
                t.translation += delta;
            }
            shifted.write(OriginShifted {
                rebase: Rebase {
                    cell_delta: [1, 0, 0],
                    shift,
                },
                origin_cell: [1, 0, 0],
            });
        };
        app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_lanes));
        app.add_systems(FixedUpdate, step_nav_lanes.before(step_nav_agents));
        app.add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
        app.world_mut().run_schedule(FixedUpdate);

        let state = app.world().get::<NavAgentState>(agent).expect("state");
        assert_eq!(state.target, Some(goal + delta));
        assert_eq!(state.waypoints, vec![goal + delta]);
    }

    /// H2 regression (audit kimirev-fix-nav-determinism, finding 1, second facet): when the agent
    /// step drains the cursor it must shift every `NavAgentState` — including parked ones the
    /// steering query filters out — or the outcome still depends on the executor's pick.
    #[test]
    fn a_parked_agents_waypoints_shift_when_the_agent_step_drains_first() {
        use crate::NavStep;
        use crate::agent::step_nav_agents;
        use regia_scene::layers::LayerParked;
        use regia_scene::prelude::Visibility;

        let shift = [1024.0, 0.0, 0.0];
        let delta = -Vec3::from_array(shift);
        let center = Vec3::new(2000.0, 0.0, 0.0);
        let goal = center + Vec3::new(4.0, 0.0, 0.0);

        let mut app = App::new();
        app.add_message::<OriginShifted>()
            .init_resource::<NavOriginShiftCursor>()
            .insert_resource(FixedTime::default())
            .insert_resource(floor(center));
        let agent = app
            .world_mut()
            .spawn((
                NavAgent::default(),
                NavAgentState {
                    target: Some(goal),
                    waypoints: vec![goal],
                    ..NavAgentState::default()
                },
                Transform::from_translation(center),
                LayerParked {
                    restore: Visibility::Inherited,
                    inherited: false,
                },
            ))
            .id();
        let runtime_rebase = move |mut transforms: Query<&mut Transform>,
                                   mut shifted: MessageWriter<OriginShifted>| {
            for mut t in &mut transforms {
                t.translation += delta;
            }
            shifted.write(OriginShifted {
                rebase: Rebase {
                    cell_delta: [1, 0, 0],
                    shift,
                },
                origin_cell: [1, 0, 0],
            });
        };
        app.add_systems(FixedUpdate, runtime_rebase.before(step_nav_agents));
        app.add_systems(FixedUpdate, step_nav_agents.in_set(NavStep));
        app.add_systems(FixedUpdate, shift_nav_origin.after(NavStep));
        app.world_mut().run_schedule(FixedUpdate);

        let state = app.world().get::<NavAgentState>(agent).expect("state");
        assert_eq!(state.target, Some(goal + delta));
        assert_eq!(state.waypoints, vec![goal + delta]);
    }
}
