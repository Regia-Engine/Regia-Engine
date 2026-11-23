// SPDX-License-Identifier: MPL-2.0
//! v0.5.1 navigation follow-ups (lane w10-nav-followups; sources: sprint-rev-claude-62 nav notes
//! and the lower-severity notes of sprint-deep-zai-nav-ai). Each test failed on integ-v051b
//! 52a1f9ca6. Fixed steps and deterministic inputs only; nothing reads the clock.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NAV_LINK_GONE, NavCrowd, NavCrowdConfig, NavLinkPhase,
    NavLinkQueueConfig, NavMesh, NavMeshBuildParams, NavMeshBuilder, NavObstacles, NavPathOutcome,
    NavTileCache, NavTileCacheParams, OffMeshLink,
};

fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

fn exclusive() -> NavCrowdConfig {
    NavCrowdConfig {
        link_queue: NavLinkQueueConfig {
            exclusive: true,
            timeout_steps: 0,
        },
        ..Default::default()
    }
}

/// Two 5 x 8 m floors with a 2 m gap.
fn gap_tris() -> Vec<[Vec3; 3]> {
    let mut tris: Vec<[Vec3; 3]> = rect(-6.0, -4.0, -1.0, 4.0, 0.0).into();
    tris.extend(rect(1.0, -4.0, 6.0, 4.0, 0.0));
    tris
}

fn gap_mesh(links: Vec<OffMeshLink>) -> NavMesh {
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&gap_tris())
    .expect("the floors build")
    .with_off_mesh_links(links)
}

/// The only way across, crossed in `seconds` (user id 9).
fn crossing(seconds: f32) -> OffMeshLink {
    let mut link = OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
        .with_user_id(9)
        .with_radius(0.5);
    link.duration = seconds;
    link
}

/// The same crossing, switched off: identical endpoints, direction, user id and kind.
fn crossing_off(seconds: f32) -> OffMeshLink {
    let mut link = crossing(seconds);
    link.enabled = false;
    link
}

// ---------------------------------------------------------------------------------------------
// Item 2: links with identical endpoints keep their pairing when the list changes.

/// One agent crosses from the take-off. The first list is `lists[0]`; once the agent is on the
/// link, the crowd steps on `lists[1]`, then `lists[2]` the step after, and so on. Returns the
/// `(link index, user id)` the crossing reported after each switch, the Finished event's link,
/// and the holders seen while the agent was on the link.
struct Run {
    after_switch: Vec<usize>,
    finished: Option<usize>,
    holder_mismatches: Vec<String>,
    state: CrowdAgentState,
}

fn cross_with_switches(lists: &[Vec<OffMeshLink>]) -> Run {
    let meshes: Vec<NavMesh> = lists.iter().map(|l| gap_mesh(l.clone())).collect();
    let mut crowd = NavCrowd::new(exclusive()).expect("config");
    let id = crowd
        .add_agent(&meshes[0], Vec3::new(-2.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&meshes[0], id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut current = 0usize;
    let mut run = Run {
        after_switch: Vec::new(),
        finished: None,
        holder_mismatches: Vec::new(),
        state: CrowdAgentState::Idle,
    };
    for k in 0..900u32 {
        crowd.step(&meshes[current]);
        for e in crowd.link_events() {
            if e.phase == NavLinkPhase::Finished {
                run.finished = Some(e.link);
            }
        }
        let on_link = crowd.link_traversals();
        if let Some((_, t)) = on_link.first() {
            if current > 0 {
                run.after_switch.push(t.link);
            }
            let holders = crowd.link_holders();
            if holders != vec![(t.link, id)] {
                run.holder_mismatches
                    .push(format!("step {k}: crossing {} holders {holders:?}", t.link));
            }
            if current + 1 < meshes.len() {
                current += 1;
            }
        }
    }
    run.state = crowd.agent(id).expect("agent").state();
    run
}

#[test]
fn a_crossing_keeps_its_link_when_an_identical_switched_off_copy_moves_behind_it() {
    // The agent crosses index 1 (index 0 is switched off). The list is then reordered: the
    // crossing is now index 0, and index 1 is the switched-off copy. Endpoints, user id and kind
    // match, so the old pairing (the k-th of the duplicates) sent the crossing to index 1.
    let run = cross_with_switches(&[
        vec![crossing_off(4.0), crossing(4.0)],
        vec![crossing(4.0), crossing_off(4.0)],
    ]);
    assert!(
        !run.after_switch.is_empty(),
        "the agent was on the link after the switch"
    );
    assert!(
        run.after_switch.iter().all(|&l| l == 0),
        "the crossing follows the link it is on: {:?}",
        run.after_switch
    );
    assert_eq!(run.finished, Some(0), "the landing names the same link");
    assert!(run.holder_mismatches.is_empty(), "{:?}", run.holder_mismatches);
    assert_eq!(run.state, CrowdAgentState::Arrived);
}

#[test]
fn a_crossing_keeps_its_link_when_an_earlier_switched_off_copy_leaves() {
    let run = cross_with_switches(&[vec![crossing_off(4.0), crossing(4.0)], vec![crossing(4.0)]]);
    assert!(
        !run.after_switch.is_empty(),
        "the agent was on the link after the switch"
    );
    assert!(
        run.after_switch.iter().all(|&l| l == 0),
        "not NAV_LINK_GONE: the link it is on is still there: {:?}",
        run.after_switch
    );
    assert_eq!(run.finished, Some(0));
    assert!(run.holder_mismatches.is_empty(), "{:?}", run.holder_mismatches);
    assert_eq!(run.state, CrowdAgentState::Arrived);
}

#[test]
fn a_crossing_survives_when_one_of_two_identical_links_leaves() {
    // Two identical copies (the level's list and a tile's streamed list can both carry one). The
    // agent took off on index 1; the copy at index 0 leaves. The copies cannot be told apart, so
    // the one left is the agent's: it must not report NAV_LINK_GONE or lose its reservation.
    let run = cross_with_switches(&[
        vec![crossing_off(4.0), crossing(4.0)],
        vec![crossing(4.0), crossing(4.0)],
        vec![crossing(4.0)],
    ]);
    assert!(
        run.after_switch.len() >= 2,
        "the agent was on the link through both switches"
    );
    assert!(
        !run.after_switch.contains(&NAV_LINK_GONE),
        "an identical copy is still there: {:?}",
        run.after_switch
    );
    assert_eq!(run.after_switch.last(), Some(&0));
    assert_eq!(run.finished, Some(0));
    assert!(run.holder_mismatches.is_empty(), "{:?}", run.holder_mismatches);
    assert_eq!(run.state, CrowdAgentState::Arrived);
}

// ---------------------------------------------------------------------------------------------
// Item 3: a streamed goal off the tile grid, beyond an unloaded tile, is not Complete.

fn stream_params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    }
}

/// A 32 m floor in 8 m tiles, streamed, with only the west half loaded; also the east cell.
fn west_half() -> (NavTileCache, Vec<u8>) {
    let tris: Vec<[Vec3; 3]> = rect(-16.0, -16.0, 16.0, 16.0, 0.0).into();
    let whole = NavTileCache::build(stream_params(), &tris).expect("builds");
    let (tx, tz) = whole.tile_grid();
    let west: Vec<(u32, u32)> = (0..tz).flat_map(|z| (0..tx / 2).map(move |x| (x, z))).collect();
    let east: Vec<(u32, u32)> = (0..tz).flat_map(|z| (tx / 2..tx).map(move |x| (x, z))).collect();
    let mut cache = NavTileCache::streamed(stream_params(), whole.layout()).expect("streamed");
    cache
        .load_cell(0, &whole.cook_cell(&west).expect("cooks"))
        .expect("loads");
    cache.update();
    (cache, whole.cook_cell(&east).expect("cooks"))
}

/// A point just past the north edge of the grid, over the first unloaded (east) tile column,
/// within the default snap box of the loaded west surface.
fn off_grid_end(cache: &NavTileCache) -> Vec3 {
    let (tx, tz) = cache.tile_grid();
    let border = cache.tile_bounds(tx / 2, 0).expect("in range").0.x;
    let north = cache.tile_bounds(0, tz - 1).expect("in range").1.z;
    Vec3::new(border + 0.5, 0.0, north + 0.25)
}

const SNAP: Vec3 = Vec3::splat(2.0);

#[test]
fn a_path_to_a_point_off_the_grid_beside_an_unloaded_tile_is_partial() {
    let (cache, _) = west_half();
    let (tx, tz) = cache.tile_grid();
    let end = off_grid_end(&cache);
    assert_eq!(cache.tile_at(end), None, "the end is off the grid: {end}");
    assert!(
        cache.query().find_nearest(end, SNAP).is_some(),
        "the end snaps onto the loaded surface"
    );
    let result = cache.find_path_streamed(Vec3::new(-10.0, 0.0, 0.0), end, SNAP);
    assert_eq!(result.outcome, NavPathOutcome::Partial, "{result:?}");
    assert_eq!(result.unloaded_tile, Some((tx / 2, tz - 1)));
    assert!(result.path.is_some());
}

#[test]
fn a_path_to_a_point_off_the_grid_beside_a_loaded_tile_is_complete() {
    let (cache, _) = west_half();
    let (_, tz) = cache.tile_grid();
    let north = cache.tile_bounds(0, tz - 1).expect("in range").1.z;
    let end = Vec3::new(-10.0, 0.0, north + 0.25);
    assert_eq!(cache.tile_at(end), None);
    let result = cache.find_path_streamed(Vec3::new(-10.0, 0.0, 0.0), end, SNAP);
    assert_eq!(result.outcome, NavPathOutcome::Complete, "{result:?}");
    assert_eq!(result.unloaded_tile, None);
}

#[test]
fn an_agent_sent_off_the_grid_beside_an_unloaded_tile_waits_for_it() {
    let (mut cache, east) = west_half();
    let (tx, tz) = cache.tile_grid();
    let end = off_grid_end(&cache);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-10.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    let outcome = crowd
        .set_target_streamed(&cache, id, end)
        .expect("a finite target");
    assert_eq!(outcome, Some(NavPathOutcome::Partial));
    for _ in 0..900 {
        cache.update();
        crowd.sync_mesh_revision(cache.revision());
        crowd.sync_streaming(&cache);
        crowd.step(cache.mesh());
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::AwaitingTile, "at {}", a.position());
    assert_eq!(a.awaiting_tile(), Some((tx / 2, tz - 1)));

    cache.load_cell(1, &east).expect("loads");
    for _ in 0..900 {
        cache.update();
        crowd.sync_mesh_revision(cache.revision());
        crowd.sync_streaming(&cache);
        crowd.step(cache.mesh());
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived, "at {}", a.position());
    assert_eq!(a.awaiting_tile(), None);
}

// ---------------------------------------------------------------------------------------------
// Item 4: a streamed replan drops the agent's place in a link queue, as a plain replan does.

#[test]
fn a_streamed_replan_drops_a_stale_link_queue_place() {
    let mut cache = NavTileCache::build(stream_params(), &gap_tris()).expect("builds");
    cache.set_off_mesh_links(vec![crossing(4.0)]);
    cache.update();
    let mut crowd = NavCrowd::new(exclusive()).expect("config");
    // A starts at the take-off, B behind it: B queues while A crosses.
    let starts = [Vec3::new(-2.0, 0.0, -0.5), Vec3::new(-4.0, 0.0, 0.5)];
    let ids: Vec<CrowdAgentId> = starts
        .iter()
        .map(|&p| crowd.add_agent(cache.mesh(), p, agent()).expect("on the mesh"))
        .collect();
    for (&id, start) in ids.iter().zip(&starts) {
        crowd
            .set_target_streamed(&cache, id, Vec3::new(4.0, 0.0, start.z))
            .expect("target");
    }
    let b = ids[1];
    let mut queued = false;
    for _ in 0..240 {
        crowd.sync_streaming(&cache);
        crowd.step(cache.mesh());
        if crowd.agent(b).expect("agent").state() == CrowdAgentState::LinkQueued {
            queued = true;
            break;
        }
    }
    assert!(queued, "B queued behind A");
    assert!(crowd.agent(b).expect("agent").link_wait().is_some());
    // The mesh changed: everyone replans; B's streamed plan runs in sync_streaming.
    crowd.request_replan_all();
    crowd.sync_streaming(&cache);
    let after = crowd.agent(b).expect("agent");
    assert_ne!(
        after.state(),
        CrowdAgentState::LinkQueued,
        "the replan put it back to walking"
    );
    assert_eq!(
        after.link_wait(),
        None,
        "a replanned agent has no queue place (plain plan clears it too)"
    );
    assert!(crowd.link_queue(0).is_empty(), "no phantom waiter in the queue");
    // B queues again with a fresh place and then crosses.
    for _ in 0..900 {
        crowd.sync_streaming(&cache);
        crowd.step(cache.mesh());
        if let Some(w) = crowd.agent(b).expect("agent").link_wait() {
            assert!(w.steps <= 240, "a fresh wait, not the old one carried on: {w:?}");
        }
    }
    for &id in &ids {
        assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    }
}

// ---------------------------------------------------------------------------------------------
// Item 5: a non-finite obstacle box is refused.

#[test]
fn a_non_finite_obstacle_box_is_not_stored() {
    let mut obstacles = NavObstacles::default();
    let nan = obstacles.add_box(Vec3::NAN, Vec3::ONE);
    let inf = obstacles.add_box(Vec3::ZERO, Vec3::new(f32::INFINITY, 1.0, 1.0));
    let neg = obstacles.add_box(Vec3::new(0.0, f32::NEG_INFINITY, 0.0), Vec3::ONE);
    assert_eq!(obstacles.obstacle_count(), 0, "nothing stored");
    for id in [nan, inf, neg] {
        assert!(!obstacles.remove(id), "the id names no obstacle");
    }
    let good = obstacles.add_box(Vec3::ZERO, Vec3::ONE);
    assert_eq!(obstacles.obstacle_count(), 1);
    assert!(![nan, inf, neg].contains(&good), "ids are not reused");
}
