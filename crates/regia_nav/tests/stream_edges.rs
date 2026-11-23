// SPDX-License-Identifier: MPL-2.0
//! Crowd agents at the edge of the streamed world (ADR-2133): a target on an unloaded tile is
//! walked to the loaded border, the agent waits there, and it goes on when the tile loads; a tile
//! that leaves under a route makes the agent re-path around it.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NavCrowd, NavCrowdConfig, NavCrowdError,
    NavMeshBuildParams, NavPathOutcome, NavTileCache, NavTileCacheParams,
};

/// 8 m tiles (32 cells of 0.25 m).
fn params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 32,
    }
}

/// A 32 m square floor around the origin.
fn floor() -> Vec<[Vec3; 3]> {
    let c = 16.0;
    vec![
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, -c),
            Vec3::new(c, 0.0, c),
        ],
        [
            Vec3::new(-c, 0.0, -c),
            Vec3::new(c, 0.0, c),
            Vec3::new(-c, 0.0, c),
        ],
    ]
}

/// A streamed cache with one cell per tile (cell id = `z * tiles_x + x`), every cell loaded, and the
/// cooked blobs to reload them.
fn world() -> (NavTileCache, Vec<Vec<u8>>, (u32, u32)) {
    let whole = NavTileCache::build(params(), &floor()).expect("builds");
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let (tx, tz) = whole.tile_grid();
    let mut blobs = Vec::new();
    for z in 0..tz {
        for x in 0..tx {
            let blob = whole.cook_cell(&[(x, z)]).expect("cooks");
            cache.load_cell(u64::from(z * tx + x), &blob).expect("loads");
            blobs.push(blob);
        }
    }
    cache.update();
    (cache, blobs, (tx, tz))
}

fn cell_of(cache: &NavTileCache, p: Vec3) -> u64 {
    let (tx, _) = cache.tile_grid();
    let (x, z) = cache.tile_at(p).expect("on the grid");
    u64::from(z * tx + x)
}

/// The host's order of calls each fixed step (what `NavCrowdPlugin` does).
fn step(crowd: &mut NavCrowd, cache: &mut NavTileCache) {
    cache.update();
    crowd.sync_mesh_revision(cache.revision());
    crowd.sync_streaming(cache);
    crowd.step(cache.mesh());
}

fn agent(crowd: &mut NavCrowd, cache: &NavTileCache, at: Vec3) -> CrowdAgentId {
    crowd
        .add_agent(cache.mesh(), at, CrowdAgentParams::default())
        .expect("on the mesh")
}

/// Unloads every tile column whose centre is east of `x` (the far side of the world).
fn unload_east_of(cache: &mut NavTileCache, x: f32, tiles: (u32, u32)) -> Vec<u64> {
    let mut gone = Vec::new();
    for tz in 0..tiles.1 {
        for tx in 0..tiles.0 {
            let id = u64::from(tz * tiles.0 + tx);
            let centre = cache.layout().origin.x + (tx as f32 + 0.5) * 8.0;
            if centre > x {
                cache.unload_cell(id).expect("loaded");
                gone.push(id);
            }
        }
    }
    gone
}

#[test]
fn a_target_on_an_unloaded_tile_is_walked_to_the_border_waited_at_and_reached_when_it_loads() {
    let (mut cache, blobs, tiles) = world();
    let gone = unload_east_of(&mut cache, 4.0, tiles);
    cache.update();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = agent(&mut crowd, &cache, Vec3::new(-10.0, 0.0, 1.0));
    let target = Vec3::new(12.0, 0.0, 1.0);
    // A plain target off the loaded surface is refused; a streamed one is not.
    assert_eq!(
        crowd.set_target(cache.mesh(), id, target),
        Err(NavCrowdError::OffMesh)
    );
    let outcome = crowd.set_target_streamed(&cache, id, target).expect("valid");
    assert_eq!(outcome, Some(NavPathOutcome::Partial));
    let waiting_for = crowd.agent(id).expect("agent").awaiting_tile().expect("a tile");
    assert_eq!(cache.tile_loaded(waiting_for.0, waiting_for.1), Some(false));

    let mut waited_at = None;
    for _ in 0..900 {
        step(&mut crowd, &mut cache);
        let a = crowd.agent(id).expect("agent");
        assert_ne!(a.state(), CrowdAgentState::NoPath);
        if a.state() == CrowdAgentState::AwaitingTile {
            waited_at = Some(a.position());
            break;
        }
    }
    let waited_at = waited_at.expect("it reached the border");
    assert!(
        waited_at.x > -1.0 && waited_at.x < 8.5,
        "by the edge: {waited_at:?}"
    );
    // It stays put while the tile is away.
    for _ in 0..120 {
        step(&mut crowd, &mut cache);
        let a = crowd.agent(id).expect("agent");
        assert_eq!(a.state(), CrowdAgentState::AwaitingTile);
        assert!(
            (a.position() - waited_at).length() < 0.05,
            "it waits: {:?}",
            a.position()
        );
        assert_eq!(a.target(), Some(target));
    }
    assert_eq!(crowd.stats().tile_waits, 1, "one wait, not one per step");

    for id in gone {
        cache.load_cell(id, &blobs[id as usize]).expect("loads");
    }
    for _ in 0..900 {
        step(&mut crowd, &mut cache);
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived, "ended at {:?}", a.position());
    assert!((a.position() - target).length() < 0.5);
    assert_eq!(a.awaiting_tile(), None);
}

#[test]
fn a_tile_that_leaves_under_the_route_makes_the_agent_re_path_around_it() {
    let (mut cache, _, _) = world();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let (start, target) = (Vec3::new(-12.0, 0.0, 1.0), Vec3::new(12.0, 0.0, 1.0));
    let id = agent(&mut crowd, &cache, start);
    assert_eq!(
        crowd.set_target_streamed(&cache, id, target).expect("valid"),
        Some(NavPathOutcome::Complete)
    );
    for _ in 0..20 {
        step(&mut crowd, &mut cache);
    }
    let replans = crowd.stats().replans;
    // The middle tile on the straight line goes: the floor around it is still loaded.
    let hole = cell_of(&cache, Vec3::new(1.0, 0.0, 1.0));
    cache.unload_cell(hole).expect("loaded");
    let mut max_dz: f32 = 0.0;
    for _ in 0..1800 {
        step(&mut crowd, &mut cache);
        let a = crowd.agent(id).expect("agent");
        assert_ne!(a.state(), CrowdAgentState::NoPath);
        assert_ne!(a.state(), CrowdAgentState::AwaitingTile, "a way around exists");
        max_dz = max_dz.max((a.position().z - 1.0).abs());
        if a.state() == CrowdAgentState::Arrived {
            break;
        }
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived, "ended at {:?}", a.position());
    assert!(crowd.stats().replans > replans, "it replanned");
    assert!(
        max_dz > 2.0,
        "it went round the hole (largest sideways step {max_dz})"
    );
}

#[test]
fn a_waiting_agent_goes_on_when_only_its_tile_returns() {
    let (mut cache, blobs, tiles) = world();
    let gone = unload_east_of(&mut cache, 4.0, tiles);
    cache.update();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = agent(&mut crowd, &cache, Vec3::new(-2.0, 0.0, 1.0));
    let target = Vec3::new(10.0, 0.0, 1.0);
    crowd.set_target_streamed(&cache, id, target).expect("valid");
    for _ in 0..600 {
        step(&mut crowd, &mut cache);
    }
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::AwaitingTile);
    let (wx, wz) = a.awaiting_tile().expect("a tile");
    let cell = u64::from(wz * tiles.0 + wx);
    assert!(gone.contains(&cell));
    cache.load_cell(cell, &blobs[cell as usize]).expect("loads");
    for _ in 0..600 {
        step(&mut crowd, &mut cache);
    }
    assert_eq!(crowd.agent(id).expect("agent").state(), CrowdAgentState::Arrived);
}

#[test]
fn streamed_walks_replay_to_the_same_digest() {
    let run = || {
        let (mut cache, blobs, tiles) = world();
        let gone = unload_east_of(&mut cache, 4.0, tiles);
        cache.update();
        let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
        let ids: Vec<_> = (0..6)
            .map(|k| agent(&mut crowd, &cache, Vec3::new(-12.0, 0.0, -6.0 + 2.0 * k as f32)))
            .collect();
        for (k, &id) in ids.iter().enumerate() {
            let target = Vec3::new(12.0, 0.0, 6.0 - 2.0 * k as f32);
            crowd.set_target_streamed(&cache, id, target).expect("valid");
        }
        let mut digests = Vec::new();
        for s in 0..1800 {
            if s == 900 {
                for &id in &gone {
                    cache.load_cell(id, &blobs[id as usize]).expect("loads");
                }
            }
            step(&mut crowd, &mut cache);
            digests.push(crowd.digest());
        }
        (digests, crowd.stats())
    };
    let (a, sa) = run();
    let (b, sb) = run();
    assert_eq!(a, b);
    assert_eq!(sa, sb);
    assert!(sa.tile_waits >= 1, "some agents waited: {sa:?}");
}

#[test]
fn a_whole_cache_plans_a_streamed_target_like_a_plain_one() {
    let whole = NavTileCache::build(params(), &floor()).expect("builds");
    let mut plain = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let mut streamed = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let (start, target) = (Vec3::new(-8.0, 0.0, -3.0), Vec3::new(9.0, 0.0, 5.0));
    let a = agent(&mut plain, &whole, start);
    let b = agent(&mut streamed, &whole, start);
    plain.set_target(whole.mesh(), a, target).expect("on the mesh");
    assert_eq!(
        streamed.set_target_streamed(&whole, b, target).expect("valid"),
        Some(NavPathOutcome::Complete)
    );
    for _ in 0..600 {
        plain.sync_streaming(&whole);
        plain.step(whole.mesh());
        streamed.sync_streaming(&whole);
        streamed.step(whole.mesh());
    }
    assert_eq!(plain.digest(), streamed.digest());
    assert_eq!(
        streamed.agent(b).expect("agent").state(),
        CrowdAgentState::Arrived
    );
}

#[test]
fn a_plain_target_or_a_clear_ends_the_streamed_wait() {
    let (mut cache, _, tiles) = world();
    unload_east_of(&mut cache, 4.0, tiles);
    cache.update();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = agent(&mut crowd, &cache, Vec3::new(-2.0, 0.0, 1.0));
    crowd
        .set_target_streamed(&cache, id, Vec3::new(12.0, 0.0, 1.0))
        .expect("valid");
    assert!(crowd.agent(id).expect("agent").streamed_target());
    crowd
        .set_target(cache.mesh(), id, Vec3::new(-6.0, 0.0, 1.0))
        .expect("on the mesh");
    let a = crowd.agent(id).expect("agent");
    assert!(!a.streamed_target());
    assert_eq!(a.awaiting_tile(), None);
    crowd
        .set_target_streamed(&cache, id, Vec3::new(12.0, 0.0, 1.0))
        .expect("valid");
    crowd.clear_target(id).expect("known");
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Idle);
    assert_eq!(a.awaiting_tile(), None);
}

#[test]
fn streamed_target_errors_are_typed() {
    let (cache, _, _) = world();
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = agent(&mut crowd, &cache, Vec3::ZERO);
    let e = crowd
        .set_target_streamed(&cache, id, Vec3::new(f32::NAN, 0.0, 0.0))
        .expect_err("not finite");
    assert_eq!(e.code(), "nav_invalid_input");
    let e = crowd
        .set_target_streamed(&cache, CrowdAgentId(77), Vec3::ZERO)
        .expect_err("unknown");
    assert_eq!(e.code(), "nav_unknown_agent");
}
