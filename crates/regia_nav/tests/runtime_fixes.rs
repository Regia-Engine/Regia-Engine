// SPDX-License-Identifier: MPL-2.0
//! v0.5.1 runtime fixes from the navigation deep review (sprint-deep-zai-nav-ai): each test names
//! its finding and failed on the v0.5.0 candidate (integ-v050rc2 7c61f9da9). Fixed steps and
//! deterministic inputs only; nothing reads the clock.

use std::f32::consts::FRAC_PI_2;

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NavBuildError, NavCrowd, NavCrowdConfig, NavLaneCrowd,
    NavLaneCrowdConfig, NavLinkPhase, NavLinkQueueConfig, NavMesh, NavMeshBuildParams, NavMeshBuilder,
    NavObstacleShape, NavTileCache, NavTileCacheError, NavTileCacheParams, NavTileLayout, OffMeshLink,
    Portal,
};

fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
    [
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z0), Vec3::new(x1, y, z1)],
        [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x0, y, z1)],
    ]
}

fn floor(half: f32) -> Vec<[Vec3; 3]> {
    rect(-half, -half, half, half, 0.0).into()
}

fn cache_params(tile_size: u32) -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size,
    }
}

/// An edit of the build parameters, one per test case.
type ParamsEdit = fn(&mut NavMeshBuildParams);

fn agent() -> CrowdAgentParams {
    CrowdAgentParams::new(0.35, 1.6, 3.0, 8.0)
}

// ---------------------------------------------------------------------------------------------
// Finding 1: agent parameters are validated.

#[test]
fn bad_agent_parameters_are_invalid_input() {
    let cases: [(&str, ParamsEdit); 14] = [
        ("radius NaN", |p| p.agent.radius = f32::NAN),
        ("radius inf", |p| p.agent.radius = f32::INFINITY),
        ("radius negative", |p| p.agent.radius = -0.1),
        ("height NaN", |p| p.agent.height = f32::NAN),
        ("height inf", |p| p.agent.height = f32::INFINITY),
        ("height zero", |p| p.agent.height = 0.0),
        ("max_climb NaN", |p| p.agent.max_climb = f32::NAN),
        ("max_climb inf", |p| p.agent.max_climb = f32::INFINITY),
        ("max_climb negative", |p| p.agent.max_climb = -0.5),
        ("max_slope NaN", |p| p.agent.max_slope = f32::NAN),
        ("max_slope inf", |p| p.agent.max_slope = f32::INFINITY),
        ("max_slope negative", |p| p.agent.max_slope = -0.1),
        ("max_slope vertical", |p| p.agent.max_slope = FRAC_PI_2),
        ("max_slope past vertical", |p| p.agent.max_slope = 1.6),
    ];
    let mut failures = Vec::new();
    for (name, edit) in cases {
        let mut params = NavMeshBuildParams::default();
        edit(&mut params);
        let built = NavMeshBuilder::new(params.clone()).build(&floor(4.0));
        if !matches!(built, Err(NavBuildError::InvalidInput(_))) {
            failures.push(format!(
                "{name}: builder gave {:?}",
                built.map(|m| m.poly_count())
            ));
        }
        let mut cache = cache_params(16);
        cache.build = params;
        let whole = NavTileCache::build(cache.clone(), &floor(4.0));
        if !matches!(
            whole,
            Err(NavTileCacheError::Build(NavBuildError::InvalidInput(_)))
        ) {
            failures.push(format!(
                "{name}: tile cache gave {:?}",
                whole.map(|c| c.revision())
            ));
        }
        let layout = NavTileLayout {
            origin: Vec3::ZERO,
            width: 32,
            height: 32,
        };
        let streamed = NavTileCache::streamed(cache, layout);
        if !matches!(
            streamed,
            Err(NavTileCacheError::Build(NavBuildError::InvalidInput(_)))
        ) {
            failures.push(format!(
                "{name}: streamed cache gave {:?}",
                streamed.map(|c| c.revision())
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // The edges of the valid ranges still build.
    let edges: [ParamsEdit; 3] = [
        |p| p.agent.radius = 0.0,
        |p| p.agent.max_climb = 0.0,
        |p| p.agent.max_slope = 0.0,
    ];
    for edit in edges {
        let mut params = NavMeshBuildParams::default();
        edit(&mut params);
        assert!(NavMeshBuilder::new(params).build(&floor(4.0)).is_ok());
    }
}

// ---------------------------------------------------------------------------------------------
// Finding 3: an origin shift or a grid growth drops a waiting partial patch.

/// A cache with a tile patch waiting: an obstacle added and `update` run, nothing consumed.
fn cache_with_waiting_patch() -> NavTileCache {
    let mut cache = NavTileCache::build(cache_params(8), &floor(6.0)).expect("builds");
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(1.0, 0.0, 1.0),
            max: Vec3::new(2.0, 2.0, 2.0),
        })
        .expect("obstacle");
    cache.update();
    assert!(cache.last_partial_revision().is_some(), "a patch waits");
    cache
}

#[test]
fn an_origin_shift_drops_a_waiting_tile_patch() {
    let mut cache = cache_with_waiting_patch();
    let before = cache.revision();
    cache.shift_origin(Vec3::new(64.0, 0.0, -32.0));
    assert_eq!(cache.revision(), before + 1);
    assert!(
        cache.take_partial().is_none(),
        "the patch described the numbering before the shift's restitch"
    );
}

#[test]
fn a_grid_growth_drops_a_waiting_tile_patch() {
    let mut cache = cache_with_waiting_patch();
    let before = cache.revision();
    let growth = cache
        .grow_to(Vec3::new(-20.0, 0.0, -20.0), Vec3::new(6.0, 1.0, 6.0))
        .expect("grows");
    assert!(!growth.unchanged);
    assert_eq!(cache.revision(), before + 1);
    assert!(
        cache.take_partial().is_none(),
        "the patch described the numbering before the growth"
    );
}

/// The whole chain: a crowd that saw the revision before the patch replans every agent after a
/// grid growth (which renumbers every tile) instead of renumbering its corridor with the stale
/// patch. The obstacle is far from the agent's corridor, so the patch alone would not replan it.
#[test]
fn a_crowd_replans_after_a_growth_that_followed_an_unconsumed_patch() {
    let mut cache = NavTileCache::build(cache_params(8), &floor(6.0)).expect("builds");
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(cache.mesh(), Vec3::new(-4.0, 0.0, -4.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(cache.mesh(), id, Vec3::new(4.0, 0.0, 4.0))
        .expect("target");
    crowd.sync_mesh_revision(cache.revision());
    crowd.step(cache.mesh());
    cache
        .add_obstacle(NavObstacleShape::Box {
            min: Vec3::new(4.5, 0.0, -5.5),
            max: Vec3::new(5.5, 2.0, -4.5),
        })
        .expect("obstacle");
    cache.update();
    cache
        .grow_to(Vec3::new(-20.0, 0.0, -20.0), Vec3::new(6.0, 1.0, 6.0))
        .expect("grows");
    // What `NavCrowdPlugin` does each step.
    if let Some(effect) = cache.take_partial() {
        crowd.sync_tile_revision(&effect);
    } else {
        crowd.sync_mesh_revision(cache.revision());
    }
    let before = crowd.stats().replans;
    crowd.step(cache.mesh());
    assert!(
        crowd.stats().replans > before,
        "the growth renumbered every polygon: the agent must replan"
    );
}

// ---------------------------------------------------------------------------------------------
// Finding 4: crowd link reservations follow their link when the link list changes.

/// Two 5 x 8 m floors with a 2 m gap; `links` over it.
fn gap_mesh(links: Vec<OffMeshLink>) -> NavMesh {
    let mut tris: Vec<[Vec3; 3]> = rect(-6.0, -4.0, -1.0, 4.0, 0.0).into();
    tris.extend(rect(1.0, -4.0, 6.0, 4.0, 0.0));
    NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("the floors build")
    .with_off_mesh_links(links)
}

/// The only way across, crossed in `seconds`.
fn crossing(seconds: f32) -> OffMeshLink {
    let mut link = OffMeshLink::new(Vec3::new(-1.5, 0.0, 0.0), Vec3::new(1.5, 0.0, 0.0))
        .with_user_id(9)
        .with_radius(0.5);
    link.duration = seconds;
    link
}

/// A switched-off link elsewhere: it only takes an index ahead of the crossing (a link of a
/// streamed tile that unloads, in a streamed world).
fn decoy() -> OffMeshLink {
    let mut link = OffMeshLink::new(Vec3::new(-5.0, 0.0, 3.5), Vec3::new(-4.0, 0.0, 3.5)).with_user_id(4);
    link.enabled = false;
    link
}

#[test]
fn a_link_reservation_follows_its_link_when_an_earlier_link_leaves() {
    let with_decoy = gap_mesh(vec![decoy(), crossing(4.0)]);
    let without = gap_mesh(vec![crossing(4.0)]);
    let mut crowd = NavCrowd::new(NavCrowdConfig {
        link_queue: NavLinkQueueConfig {
            exclusive: true,
            timeout_steps: 0,
        },
        ..Default::default()
    })
    .expect("config");
    // A starts at the take-off, B well behind it: A is on the link long before B arrives.
    let starts = [Vec3::new(-2.0, 0.0, -0.5), Vec3::new(-5.5, 0.0, 0.5)];
    let ids: Vec<CrowdAgentId> = starts
        .iter()
        .map(|&p| crowd.add_agent(&with_decoy, p, agent()).expect("on the mesh"))
        .collect();
    for (&id, start) in ids.iter().zip(&starts) {
        crowd
            .set_target(&with_decoy, id, Vec3::new(4.0, 0.0, start.z))
            .expect("target");
    }
    let mut mesh = &with_decoy;
    let mut swapped_at = None;
    let mut started = Vec::new();
    for k in 0..900u32 {
        crowd.step(mesh);
        for e in crowd.link_events() {
            if e.phase == NavLinkPhase::Started {
                started.push((k, e.agent, e.link));
            }
        }
        let on_link: Vec<CrowdAgentId> = crowd
            .agents()
            .iter()
            .filter(|a| a.state() == CrowdAgentState::OffMeshLink)
            .map(|a| a.id())
            .collect();
        assert!(
            on_link.len() <= 1,
            "step {k}: {on_link:?} share the reserved link"
        );
        let holders: Vec<CrowdAgentId> = crowd.link_holders().iter().map(|h| h.1).collect();
        assert_eq!(holders, on_link, "step {k}: the holder is the agent on the link");
        if swapped_at.is_none() && !on_link.is_empty() {
            // The decoy leaves while A crosses: the crossing moves from index 1 to index 0.
            mesh = &without;
            swapped_at = Some(k);
        }
        if let Some(s) = swapped_at
            && k > s
        {
            for (_, t) in crowd.link_traversals() {
                assert_eq!(t.link, 0, "step {k}: the crossing follows its link");
                assert_eq!(t.user_id, 9);
            }
            for &(l, _) in &crowd.link_holders() {
                assert_eq!(l, 0, "step {k}: the reservation follows its link");
            }
        }
    }
    assert!(swapped_at.is_some(), "A reached the link");
    assert_eq!(started.len(), 2, "both crossed, one after the other: {started:?}");
    assert_eq!(started[0].2, 1, "A took off on index 1 of the first list");
    assert_eq!(started[1].2, 0, "B took off on index 0 of the second list");
    for &id in &ids {
        assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
    }
}

// ---------------------------------------------------------------------------------------------
// Finding 6: a huge but finite vertex in a region rebuild does not overflow.

#[test]
fn a_region_rebuild_with_a_huge_finite_vertex_does_not_panic() {
    let mut cache = NavTileCache::build(cache_params(8), &floor(6.0)).expect("builds");
    let mut tris = floor(6.0);
    for far in [4e18f32, -4e18, 3.0e38, -3.0e38] {
        tris.push([
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(far, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ]);
        tris.push([
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, far),
        ]);
    }
    let result = cache
        .rebuild_region(Vec3::new(-2.0, -1.0, -2.0), Vec3::new(2.0, 1.0, 2.0), &tris)
        .expect("finite geometry is accepted");
    assert!(!result.outside);
    cache.update();
    assert!(cache.mesh().poly_count() > 0);
}

// ---------------------------------------------------------------------------------------------
// Finding 9: links set in code are validated; a crossing always takes at least one step.

#[test]
fn the_tile_cache_leaves_out_invalid_links() {
    let mut cache = NavTileCache::build(cache_params(8), &floor(6.0)).expect("builds");
    let good = OffMeshLink::new(Vec3::new(-3.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)).with_user_id(1);
    let mut nan = good.with_user_id(2);
    nan.from.x = f32::NAN;
    let mut negative = good.with_user_id(3);
    negative.duration = -1.0;
    let mut area = good.with_user_id(4);
    area.area = 64;
    let zero_radius = good.with_user_id(5).with_radius(0.0);
    let all = vec![nan, good, negative, area, zero_radius];
    // The strict setter refuses the list and changes nothing.
    let revision = cache.revision();
    assert!(cache.try_set_off_mesh_links(all.clone()).is_err());
    assert!(cache.off_mesh_links().is_empty());
    assert_eq!(cache.revision(), revision);
    let mut area_only = good;
    area_only.area = 64;
    assert!(cache.try_set_off_mesh_links(vec![good, area_only]).is_err());
    // The plain setter keeps the valid ones in order.
    cache.set_off_mesh_links(all);
    let kept: Vec<u64> = cache.off_mesh_links().iter().map(|l| l.user_id).collect();
    assert_eq!(kept, vec![1]);
    assert!(cache.try_set_off_mesh_links(vec![good]).is_ok());
}

/// A crowd crossing one link with `duration` (set straight on the mesh, which does not check
/// it): the take-off's step count and the steps from take-off to landing.
fn crossing_steps(duration: f32) -> (u32, u32) {
    let mesh = gap_mesh(vec![crossing(duration)]);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(&mesh, Vec3::new(-3.0, 0.0, 0.0), agent())
        .expect("on the mesh");
    crowd
        .set_target(&mesh, id, Vec3::new(4.0, 0.0, 0.0))
        .expect("target");
    let mut took_off = None;
    for k in 0..2000u32 {
        crowd.step(&mesh);
        for e in crowd.link_events() {
            match e.phase {
                NavLinkPhase::Started => {
                    let total = crowd.link_takeoffs()[0].total_steps;
                    took_off = Some((k, total));
                }
                NavLinkPhase::Finished => {
                    let (at, total) = took_off.expect("took off before landing");
                    return (total, k - at);
                }
                _ => {}
            }
        }
    }
    panic!("the agent never crossed (took off: {took_off:?})");
}

#[test]
fn a_non_finite_link_duration_still_crosses_in_steps() {
    let (total, steps) = crossing_steps(f32::NAN);
    assert!(total >= 1, "NaN duration: {total} steps");
    assert!(steps >= 1, "NaN duration landed in the take-off step");
    let (normal, _) = crossing_steps(0.0);
    assert_eq!(total, normal, "a NaN duration reads as no authored duration");
    let (total, steps) = crossing_steps(f32::INFINITY);
    assert_eq!(
        total, normal,
        "an infinite duration reads as no authored duration"
    );
    assert!(steps <= normal + 1);
    let (total, _) = crossing_steps(-2.0);
    assert_eq!(total, normal);
}

// ---------------------------------------------------------------------------------------------
// Finding 11: `max_steps_per_advance` has a ceiling.

#[test]
fn max_steps_per_advance_has_a_ceiling() {
    for steps in [regia_nav::MAX_STEPS_PER_ADVANCE + 1, u32::MAX] {
        assert!(
            NavCrowd::new(NavCrowdConfig {
                max_steps_per_advance: steps,
                ..Default::default()
            })
            .is_err(),
            "crowd {steps}"
        );
        assert!(
            NavLaneCrowd::new(NavLaneCrowdConfig {
                max_steps_per_advance: steps,
                ..Default::default()
            })
            .is_err(),
            "lane crowd {steps}"
        );
    }
    for steps in [0, 1, regia_nav::MAX_STEPS_PER_ADVANCE] {
        assert!(
            NavCrowd::new(NavCrowdConfig {
                max_steps_per_advance: steps,
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            NavLaneCrowd::new(NavLaneCrowdConfig {
                max_steps_per_advance: steps,
                ..Default::default()
            })
            .is_ok()
        );
    }
    // The ceiling bounds one long frame: a 10-minute delta runs at most the ceiling.
    let mesh = NavMeshBuilder::default().build(&floor(4.0)).expect("builds");
    let mut crowd = NavCrowd::new(NavCrowdConfig {
        max_steps_per_advance: regia_nav::MAX_STEPS_PER_ADVANCE,
        ..Default::default()
    })
    .expect("config");
    assert_eq!(
        crowd.advance(&mesh, 600.0).expect("advance"),
        regia_nav::MAX_STEPS_PER_ADVANCE
    );
}

// ---------------------------------------------------------------------------------------------
// Finding 12: a portal naming a missing polygon is a wall for the crowd too.

#[test]
fn a_crowd_survives_a_portal_to_a_missing_polygon() {
    // A floor with a hole in the middle, so it holds several polygons joined by portals.
    let mut tris: Vec<[Vec3; 3]> = Vec::new();
    tris.extend(rect(-6.0, -6.0, 6.0, -2.0, 0.0));
    tris.extend(rect(-6.0, 2.0, 6.0, 6.0, 0.0));
    tris.extend(rect(-6.0, -2.0, -2.0, 2.0, 0.0));
    tris.extend(rect(2.0, -2.0, 6.0, 2.0, 0.0));
    let mut mesh = NavMeshBuilder::new(NavMeshBuildParams {
        cell_size: 0.25,
        ..Default::default()
    })
    .build(&tris)
    .expect("the ring builds");
    assert!(mesh.poly_count() > 1);
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let ids: Vec<CrowdAgentId> = [
        (Vec3::new(-4.0, 0.0, -4.0), Vec3::new(4.0, 0.0, 4.0)),
        (Vec3::new(4.0, 0.0, -4.0), Vec3::new(-4.0, 0.0, 4.0)),
    ]
    .iter()
    .map(|&(from, to)| {
        let id = crowd.add_agent(&mesh, from, agent()).expect("on the mesh");
        crowd.set_target(&mesh, id, to).expect("target");
        id
    })
    .collect();
    // Plan on the sound mesh, then damage it: every portal gets a twin in front of it that names a
    // polygon the mesh does not hold.
    crowd.step(&mesh);
    let mut damaged = 0;
    for poly in &mut mesh.polys {
        let twins: Vec<Portal> = poly
            .portals
            .iter()
            .map(|p| Portal {
                neighbor: 9999,
                a: p.a,
                b: p.b,
            })
            .collect();
        damaged += twins.len();
        let real = std::mem::take(&mut poly.portals);
        poly.portals = twins.into_iter().chain(real).collect();
    }
    assert!(damaged > 0);
    for _ in 0..600 {
        crowd.step(&mesh);
    }
    for id in ids {
        let a = crowd.agent(id).expect("agent");
        assert!(a.position().is_finite());
    }
}
