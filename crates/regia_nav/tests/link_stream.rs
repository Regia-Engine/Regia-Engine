// SPDX-License-Identifier: MPL-2.0
//! Off-mesh links streamed per tile (ADR-2589, nav cell v2): a link lives in the cooked bytes of
//! the tile under its `from` point, joins the mesh when that tile loads and leaves when it
//! unloads; a link whose landing tile is not loaded is skipped by the path query until it loads;
//! an agent mid-link when its landing tile unloads goes back to the take-off point. Fixed steps
//! and deterministic inputs only.

use regia_core::math::Vec3;
use regia_nav::{
    CrowdAgentId, CrowdAgentParams, CrowdAgentState, NAV_CELL_FORMAT_VERSION, NavCrowd, NavCrowdConfig,
    NavLinkKind, NavLinkPhase, NavLinkState, NavMeshBuildParams, NavPathOutcome, NavTileCache,
    NavTileCacheParams, OffMeshLink,
};

/// 4 m tiles (16 cells of 0.25 m).
fn params() -> NavTileCacheParams {
    NavTileCacheParams {
        build: NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        },
        tile_size: 16,
    }
}

fn rect(x0: f32, z0: f32, x1: f32, z1: f32) -> [[Vec3; 3]; 2] {
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

/// Two floors, `x ∈ [-14, -1]` and `x ∈ [1, 14]`, `z ∈ [-3, 3]`, with a 2 m gap.
fn islands() -> Vec<[Vec3; 3]> {
    let mut t: Vec<[Vec3; 3]> = rect(-14.0, -3.0, -1.0, 3.0).into();
    t.extend(rect(1.0, -3.0, 14.0, 3.0));
    t
}

const FROM: Vec3 = Vec3::new(-1.5, 0.0, 0.0);
const TO: Vec3 = Vec3::new(6.0, 0.0, 0.0);

/// The one-way link over the gap: from the left floor's edge onto the right floor, 7.5 m: its
/// ends lie on different 4 m tiles.
fn gap_link() -> OffMeshLink {
    let mut l = OffMeshLink::new(FROM, TO)
        .one_way()
        .with_user_id(42)
        .with_radius(0.5);
    l.duration = 0.5;
    l
}

/// The cooked world: a whole cache with the link, and one cooked cell per tile (cell id =
/// `z * tiles_x + x`).
fn cooked() -> (NavTileCache, Vec<Vec<u8>>) {
    let mut whole = NavTileCache::build(params(), &islands()).expect("builds");
    whole.set_off_mesh_links(vec![gap_link()]);
    let (tx, tz) = whole.tile_grid();
    let mut blobs = Vec::new();
    for z in 0..tz {
        for x in 0..tx {
            blobs.push(whole.cook_cell(&[(x, z)]).expect("cooks"));
        }
    }
    (whole, blobs)
}

fn cell_of(cache: &NavTileCache, p: Vec3) -> u64 {
    let (tx, _) = cache.tile_grid();
    let (x, z) = cache.tile_at(p).expect("on the grid");
    u64::from(z * tx + x)
}

/// A streamed cache with every cell but `skip` loaded and the mesh up to date.
fn streamed(blobs: &[Vec<u8>], whole: &NavTileCache, skip: &[u64]) -> NavTileCache {
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    for (i, blob) in blobs.iter().enumerate() {
        if !skip.contains(&(i as u64)) {
            cache.load_cell(i as u64, blob).expect("loads");
        }
    }
    cache.update();
    cache
}

fn link_state(cache: &NavTileCache) -> Vec<NavLinkState> {
    cache.mesh().link_statuses().iter().map(|s| s.state).collect()
}

fn extents() -> Vec3 {
    Vec3::new(1.0, 1.0, 1.0)
}

#[test]
fn a_link_belongs_to_its_start_tile_and_streams_with_it() {
    let (whole, blobs) = cooked();
    let (from_tile, to_tile) = (
        whole.tile_at(FROM).expect("on grid"),
        whole.tile_at(TO).expect("on grid"),
    );
    assert_ne!(from_tile, to_tile, "the link crosses a tile border");
    assert_eq!(whole.link_tile(&gap_link()), Some(from_tile));
    // Links stream since v2 (ADR-2589); v3 (ADR-8899) keeps the layout.
    assert_eq!(NAV_CELL_FORMAT_VERSION, 3);
    // Only the take-off tile's cell carries it.
    let (from_cell, to_cell) = (cell_of(&whole, FROM), cell_of(&whole, TO));
    for (i, blob) in blobs.iter().enumerate() {
        let mut probe = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
        probe.load_cell(i as u64, blob).expect("loads");
        probe.update();
        let expected = usize::from(i as u64 == from_cell);
        assert_eq!(probe.mesh().off_mesh_links.len(), expected, "cell {i}");
    }
    // Loaded with its tile, gone with it; the cache's own list stays empty throughout.
    let mut cache = streamed(&blobs, &whole, &[]);
    assert_eq!(cache.tile_links(from_tile.0, from_tile.1), &[gap_link()]);
    assert!(cache.off_mesh_links().is_empty());
    assert!(matches!(link_state(&cache)[..], [NavLinkState::Active { .. }]));
    let rows = cache.link_tiles();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].tile,
            rows[0].loaded,
            rows[0].links,
            rows[0].streamed,
            rows[0].active
        ),
        (from_tile, true, 1, 1, 1)
    );
    cache.unload_cell(from_cell).expect("loaded");
    assert!(cache.tile_links(from_tile.0, from_tile.1).is_empty());
    cache.update();
    assert!(cache.mesh().off_mesh_links.is_empty());
    assert!(cache.link_tiles().is_empty());
    cache
        .load_cell(from_cell, &blobs[from_cell as usize])
        .expect("reloads");
    cache.update();
    assert_eq!(cache.mesh().off_mesh_links, vec![gap_link()]);
    // Its landing tile's cell has no say in it.
    cache.unload_cell(to_cell).expect("loaded");
    cache.update();
    assert_eq!(cache.mesh().off_mesh_links.len(), 1);
}

#[test]
fn a_link_into_an_unloaded_tile_is_skipped_until_the_tile_loads() {
    let (whole, blobs) = cooked();
    let to_cell = cell_of(&whole, TO);
    let mut cache = streamed(&blobs, &whole, &[to_cell]);
    // The landing tile is gone: the link is not an edge, even though its 0.5 m snap radius would
    // reach nothing else, and a nearby loaded polygon would not count either.
    assert_eq!(
        link_state(&cache),
        vec![NavLinkState::Unresolved {
            from: true,
            to: false
        }]
    );
    let start = Vec3::new(-8.0, 0.0, 0.0);
    let goal = Vec3::new(10.0, 0.0, 0.0);
    let path = cache.find_path_streamed(start, goal, extents());
    assert_ne!(path.outcome, NavPathOutcome::Complete, "{path:?}");
    if let Some(p) = &path.path {
        assert!(
            p.waypoints.iter().all(|w| w.x < 0.0),
            "the path stays on the left floor: {p:?}"
        );
    }
    let rows = cache.link_tiles();
    assert_eq!((rows[0].links, rows[0].active, rows[0].waiting), (1, 0, 1));
    // The tile loads: the link is an edge again and the path takes it.
    cache.load_cell(to_cell, &blobs[to_cell as usize]).expect("loads");
    cache.update();
    assert!(matches!(link_state(&cache)[..], [NavLinkState::Active { .. }]));
    let path = cache.find_path_streamed(start, goal, extents());
    assert_eq!(path.outcome, NavPathOutcome::Complete);
    let waypoints = path.path.expect("a path").waypoints;
    assert!(
        waypoints.iter().any(|w| (*w - FROM).length() < 1e-3),
        "{waypoints:?}"
    );
    assert!(waypoints.iter().any(|w| (*w - TO).length() < 1e-3));
    assert_eq!(cache.link_tiles()[0].waiting, 0);
}

#[test]
fn switching_a_streamed_link_off_and_shifting_the_origin_reach_the_tile_links() {
    let (whole, blobs) = cooked();
    let mut cache = streamed(&blobs, &whole, &[]);
    assert_eq!(cache.set_link_enabled(42, false), 1);
    assert_eq!(link_state(&cache), vec![NavLinkState::Disabled]);
    assert_eq!(cache.set_link_enabled(42, true), 1);
    let from_tile = cache.tile_at(FROM).expect("on grid");
    let delta = Vec3::new(64.0, 2.0, -32.0);
    cache.shift_origin(delta);
    cache.update();
    let link = cache.tile_links(from_tile.0, from_tile.1)[0];
    assert_eq!((link.from, link.to), (FROM + delta, TO + delta));
    assert!(matches!(link_state(&cache)[..], [NavLinkState::Active { .. }]));
    // Re-cooking the shifted cache writes the link back in the cook's frame.
    let again = cache.cook_cell(&[from_tile]).expect("cooks");
    assert_eq!(
        again,
        blobs[cell_of(&whole, FROM) as usize],
        "same bytes as the first cook"
    );
}

/// FNV-1a, as the nav cell trailer.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `blob` with its checksum recomputed.
fn resealed(mut blob: Vec<u8>) -> Vec<u8> {
    let body = blob.len() - 8;
    let sum = fnv1a(&blob[..body]);
    blob[body..].copy_from_slice(&sum.to_le_bytes());
    blob
}

/// A v1 cell of one tile: the v2 bytes of a tile without links, less its link count, version 1.
fn as_v1(v2: &[u8], cells: usize) -> Vec<u8> {
    let tile_end = 20 + 8 + cells * 5;
    assert_eq!(
        &v2[tile_end..tile_end + 4],
        &0u32.to_le_bytes(),
        "no links on this tile"
    );
    let mut v1 = v2[..tile_end].to_vec();
    v1.extend_from_slice(&v2[tile_end + 4..]);
    v1[4..8].copy_from_slice(&1u32.to_le_bytes());
    resealed(v1)
}

fn cells_of(cache: &NavTileCache, tile: (u32, u32)) -> usize {
    let (lo, hi) = cache.tile_bounds(tile.0, tile.1).expect("in range");
    (((hi.x - lo.x) / 0.25).round() * ((hi.z - lo.z) / 0.25).round()) as usize
}

#[test]
fn a_v1_cell_loads_with_no_links_and_bad_link_bytes_are_refused() {
    let (whole, blobs) = cooked();
    let to_cell = cell_of(&whole, TO);
    let to_tile = whole.tile_at(TO).expect("on grid");
    let v1 = as_v1(&blobs[to_cell as usize], cells_of(&whole, to_tile));
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let load = cache.load_cell(to_cell, &v1).expect("a v1 cell loads");
    assert_eq!(load.added, vec![to_tile]);
    cache.update();
    let mut fresh = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    fresh.load_cell(to_cell, &blobs[to_cell as usize]).expect("loads");
    fresh.update();
    assert_eq!(
        cache.mesh().polys.len(),
        fresh.mesh().polys.len(),
        "same tile as the v2 cell"
    );
    assert_eq!(
        cache.cook_cell(&[to_tile]).expect("cooks"),
        blobs[to_cell as usize],
        "re-cooks as v2"
    );

    // The link tile's record: kind 9 is unknown, a bad bool is refused, a count past the bytes too.
    let from_cell = cell_of(&whole, FROM);
    let from_tile = whole.tile_at(FROM).expect("on grid");
    let good = &blobs[from_cell as usize];
    let record = 20 + 8 + cells_of(&whole, from_tile) * 5 + 4;
    let mut bad_kind = good.clone();
    bad_kind[record + 26..record + 28].copy_from_slice(&9u16.to_le_bytes());
    let mut bad_bool = good.clone();
    bad_bool[record + 24] = 7;
    let mut bad_count = good.clone();
    bad_count[record - 4..record].copy_from_slice(&1000u32.to_le_bytes());
    let mut bad_radius = good.clone();
    bad_radius[record + 41..record + 45].copy_from_slice(&(-1.0f32).to_bits().to_le_bytes());
    for (name, blob) in [
        ("kind", bad_kind),
        ("bool", bad_bool),
        ("count", bad_count),
        ("radius", bad_radius),
    ] {
        let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
        let e = cache.load_cell(from_cell, &resealed(blob)).expect_err(name);
        assert_eq!(e.code(), "nav_cell_corrupt", "{name}: {e}");
        assert!(cache.loaded_cells().is_empty(), "{name}: nothing applied");
    }
    // A newer version is refused as before.
    let mut newer = good.clone();
    newer[4..8].copy_from_slice(&(NAV_CELL_FORMAT_VERSION + 1).to_le_bytes());
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    let e = cache.load_cell(from_cell, &resealed(newer)).expect_err("newer");
    assert_eq!(e.code(), "nav_cell_version");
    // The record round-trips every field.
    let mut link = gap_link().with_kind(NavLinkKind::Custom(77)).with_flags(6);
    link.area = 3;
    link.cost_scale = 2.5;
    link.enabled = false;
    let mut source = NavTileCache::build(params(), &islands()).expect("builds");
    source.set_off_mesh_links(vec![link]);
    let blob = source.cook_cell(&[from_tile]).expect("cooks");
    let mut cache = NavTileCache::streamed(params(), whole.layout()).expect("streamed");
    cache.load_cell(0, &blob).expect("loads");
    assert_eq!(cache.tile_links(from_tile.0, from_tile.1), &[link]);
}

/// The host's order of calls each fixed step (what `NavCrowdPlugin` does).
fn step(crowd: &mut NavCrowd, cache: &mut NavTileCache) {
    cache.update();
    crowd.sync_mesh_revision(cache.revision());
    crowd.sync_streaming(cache);
    crowd.step(cache.mesh());
}

fn crossing_agent(cache: &NavTileCache) -> (NavCrowd, CrowdAgentId) {
    let mut crowd = NavCrowd::new(NavCrowdConfig::default()).expect("config");
    let id = crowd
        .add_agent(
            cache.mesh(),
            Vec3::new(-6.0, 0.0, 0.0),
            CrowdAgentParams::default(),
        )
        .expect("on the mesh");
    crowd
        .set_target_streamed(cache, id, Vec3::new(10.0, 0.0, 0.0))
        .expect("target");
    (crowd, id)
}

/// Steps until the agent is on the link; returns the steps taken.
fn until_on_link(crowd: &mut NavCrowd, cache: &mut NavTileCache, id: CrowdAgentId) -> u32 {
    for k in 0..600 {
        step(crowd, cache);
        if crowd.agent(id).expect("agent").link_traversal().is_some() {
            return k;
        }
    }
    panic!("never reached the link");
}

#[test]
fn an_agent_mid_link_goes_back_when_its_landing_tile_unloads() {
    let run = || {
        let (whole, blobs) = cooked();
        let mut cache = streamed(&blobs, &whole, &[]);
        let (mut crowd, id) = crossing_agent(&cache);
        until_on_link(&mut crowd, &mut cache, id);
        step(&mut crowd, &mut cache);
        let t = *crowd
            .agent(id)
            .expect("agent")
            .link_traversal()
            .expect("still crossing");
        let to_cell = cell_of(&whole, TO);
        cache.unload_cell(to_cell).expect("loaded");
        step(&mut crowd, &mut cache);
        let events: Vec<_> = crowd.link_events().to_vec();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            (events[0].phase, events[0].user_id),
            (NavLinkPhase::Cancelled, 42)
        );
        assert_eq!(events[0].position, t.start);
        assert_eq!(crowd.stats().link_cancellations, 1);
        let a = crowd.agent(id).expect("agent");
        assert!(a.link_traversal().is_none());
        assert_ne!(a.state(), CrowdAgentState::OffMeshLink);
        assert!(
            a.position().x < 0.0,
            "back on the take-off side: {:?}",
            a.position()
        );
        let mut digests = Vec::new();
        // While the tile is away nobody crosses, and the agent never stands on it.
        for _ in 0..120 {
            step(&mut crowd, &mut cache);
            let a = crowd.agent(id).expect("agent");
            assert!(a.link_traversal().is_none() && a.position().x < 0.0);
            digests.push(crowd.digest());
        }
        // The tile returns: the agent crosses and arrives.
        cache
            .load_cell(to_cell, &blobs[to_cell as usize])
            .expect("reloads");
        let mut finished = 0;
        for _ in 0..900 {
            step(&mut crowd, &mut cache);
            finished += crowd
                .link_events()
                .iter()
                .filter(|e| e.phase == NavLinkPhase::Finished)
                .count();
            digests.push(crowd.digest());
        }
        let a = crowd.agent(id).expect("agent");
        assert_eq!(finished, 1);
        assert_eq!(a.state(), CrowdAgentState::Arrived, "{:?}", a.position());
        assert!((a.position() - Vec3::new(10.0, 0.0, 0.0)).length() < 0.5);
        digests
    };
    assert_eq!(run(), run(), "two runs give the same crowd digests");
}

#[test]
fn an_agent_mid_link_lands_when_its_take_off_tile_unloads() {
    let (whole, blobs) = cooked();
    let mut cache = streamed(&blobs, &whole, &[]);
    let (mut crowd, id) = crossing_agent(&cache);
    until_on_link(&mut crowd, &mut cache, id);
    cache.unload_cell(cell_of(&whole, FROM)).expect("loaded");
    let mut phases = Vec::new();
    for _ in 0..600 {
        step(&mut crowd, &mut cache);
        phases.extend(crowd.link_events().iter().map(|e| e.phase));
    }
    assert_eq!(
        phases,
        vec![NavLinkPhase::Finished],
        "it lands: the take-off tile is behind it"
    );
    let a = crowd.agent(id).expect("agent");
    assert_eq!(a.state(), CrowdAgentState::Arrived);
    assert_eq!(crowd.stats().link_cancellations, 0);
}
