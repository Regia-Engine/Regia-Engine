// SPDX-License-Identifier: MPL-2.0
//! Lane preference and opposing flow (ADR-2133, `.navlanes.ron` v2): weighted junction choices,
//! preferred lane bits, weighted routes, and agents keeping right of oncoming agents on a paired
//! lane.

use regia_core::math::Vec3;
use regia_nav::{
    NAV_LANES_FORMAT_VERSION, NavLaneAgentId, NavLaneAgentParams, NavLaneCrowd, NavLaneCrowdConfig,
    NavLaneDef, NavLaneGraph, NavLaneSet, migrate_nav_lanes, nav_lanes_migration_sources,
};

fn lane(points: &[(f32, f32)]) -> NavLaneDef {
    let p: Vec<Vec3> = points.iter().map(|&(x, z)| Vec3::new(x, 0.0, z)).collect();
    NavLaneDef::new(&p)
}

fn crowd() -> NavLaneCrowd {
    NavLaneCrowd::new(NavLaneCrowdConfig::default()).expect("valid config")
}

/// A lane splitting at x = 10 into lanes 1 (north) and 2 (south), both dead ends.
fn split(w1: f32, w2: f32, flags: (u16, u16)) -> NavLaneGraph {
    let mut a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
    a.next = vec![1, 2];
    let mut north = lane(&[(10.0, 0.0), (20.0, 10.0)]);
    north.weight = w1;
    north.flags = flags.0;
    let mut south = lane(&[(10.0, 0.0), (20.0, -10.0)]);
    south.weight = w2;
    south.flags = flags.1;
    NavLaneGraph::build(&[a, north, south], 0.0).expect("valid lanes")
}

/// Where each of `n` wanderers (seeds `0..n`, each alone) turns at the split.
fn turns(graph: &NavLaneGraph, n: u64, params: NavLaneAgentParams) -> Vec<u32> {
    (0..n)
        .map(|seed| {
            let mut c = crowd();
            let id: NavLaneAgentId = c
                .add_agent(graph, 0, 9.9, 0.0, params, seed)
                .expect("valid agent");
            for _ in 0..30 {
                c.step(graph);
            }
            c.agent(id).expect("agent").lane()
        })
        .collect()
}

#[test]
fn weights_split_wanderers_in_proportion() {
    let graph = split(3.0, 1.0, (1, 1));
    let picks = turns(&graph, 400, NavLaneAgentParams::default());
    let north = picks.iter().filter(|&&l| l == 1).count();
    assert!(picks.iter().all(|&l| l == 1 || l == 2), "{picks:?}");
    // 300 expected; a deterministic hash, so the bound is generous but far from the 200 of v1.
    assert!((260..=340).contains(&north), "north took {north} of 400");
}

#[test]
fn equal_weights_pick_exactly_as_unweighted_lanes() {
    let plain = turns(&split(1.0, 1.0, (1, 1)), 200, NavLaneAgentParams::default());
    let doubled = turns(&split(2.0, 2.0, (1, 1)), 200, NavLaneAgentParams::default());
    assert_eq!(plain, doubled);
    let north = plain.iter().filter(|&&l| l == 1).count();
    assert!((60..=140).contains(&north), "an even split: {north}");
}

#[test]
fn preferred_bits_win_while_such_a_lane_is_open() {
    let mut graph = split(1.0, 1.0, (1, 1 | 4));
    let params = NavLaneAgentParams {
        prefer_flags: 4,
        ..Default::default()
    };
    let picks = turns(&graph, 50, params);
    assert!(picks.iter().all(|&l| l == 2), "{picks:?}");
    // Closed, the preferred lane is no choice; the others remain.
    let mut defs = vec![
        lane(&[(0.0, 0.0), (10.0, 0.0)]),
        lane(&[(10.0, 0.0), (20.0, 10.0)]),
        lane(&[(10.0, 0.0), (20.0, -10.0)]),
    ];
    defs[0].next = vec![1, 2];
    defs[2].flags = 1 | 4;
    defs[2].user_id = 9;
    graph = NavLaneGraph::build(&defs, 0.0).expect("valid lanes");
    graph.set_lane_enabled(9, false);
    let picks = turns(&graph, 50, params);
    assert!(picks.iter().all(|&l| l == 1), "{picks:?}");
}

#[test]
fn a_route_takes_the_preferred_lane_when_its_weighted_time_is_lower() {
    // Two ways from the split to the merge: 1 is short, 2 is longer.
    let build = |w_long: f32| {
        let mut a = lane(&[(0.0, 0.0), (10.0, 0.0)]);
        a.next = vec![1, 2];
        let mut short = lane(&[(10.0, 0.0), (30.0, 0.0)]);
        short.next = vec![3];
        let mut long = lane(&[(10.0, 0.0), (20.0, 8.0), (30.0, 0.0)]);
        long.next = vec![3];
        long.weight = w_long;
        let out = lane(&[(30.0, 0.0), (40.0, 0.0)]);
        NavLaneGraph::build(&[a, short, long, out], 0.0).expect("valid lanes")
    };
    let plain = build(1.0).find_route(0, 3, 1).expect("a route");
    assert_eq!(plain.lanes, vec![0, 1, 3]);
    let preferred = build(2.0).find_route(0, 3, 1).expect("a route");
    assert_eq!(preferred.lanes, vec![0, 2, 3]);
    assert!(preferred.length > plain.length, "the length is still metres");
}

/// Lane 0 runs east along z = 0, lane 1 back west over the same strip; paired or not.
fn two_way(paired: bool, width: f32) -> NavLaneGraph {
    let mut east = lane(&[(0.0, 0.0), (30.0, 0.0)]);
    east.width = width;
    let mut west = lane(&[(30.0, 0.0), (0.0, 0.0)]);
    west.width = width;
    if paired {
        west.opposite = Some(0);
    }
    NavLaneGraph::build(&[east, west], 0.0).expect("valid lanes")
}

/// Walks one agent each way head on and returns the closest the two came and the crowd.
fn head_on(graph: &NavLaneGraph) -> (f32, NavLaneCrowd) {
    let mut c = crowd();
    let p = NavLaneAgentParams::default();
    let a = c.add_agent(graph, 0, 0.0, 0.0, p, 1).expect("valid agent");
    let b = c.add_agent(graph, 1, 0.0, 0.0, p, 2).expect("valid agent");
    let mut closest = f32::INFINITY;
    for _ in 0..900 {
        c.step(graph);
        let (pa, pb) = (
            c.position(graph, a).expect("placed"),
            c.position(graph, b).expect("placed"),
        );
        closest = closest.min((pa - pb).length());
    }
    (closest, c)
}

#[test]
fn paired_lanes_make_oncoming_agents_keep_right_and_pass_clear() {
    let graph = two_way(true, 3.0);
    assert_eq!(
        graph.lane(0).expect("lane").opposite,
        Some(1),
        "pairing is mutual"
    );
    let (closest, c) = head_on(&graph);
    assert!(closest >= 0.6 - 1e-3, "bodies of 0.3 m passed {closest} m apart");
    assert!(c.stats().oncoming_yields > 0);
    // Both reached their ends: nobody was held up for good.
    assert!(
        c.agents().iter().all(|a| a.distance() >= 29.9),
        "{:?}",
        c.agents()
    );
    // Each kept to its own right.
    assert!(c.agents().iter().all(|a| a.offset() > 0.0), "{:?}", c.agents());

    let (unpaired, c) = head_on(&two_way(false, 3.0));
    assert!(unpaired < 0.3, "unpaired lanes do not see each other: {unpaired}");
    assert_eq!(c.stats().oncoming_yields, 0);
}

#[test]
fn opposing_flows_replay_to_the_same_digest() {
    let run = || {
        let graph = two_way(true, 4.0);
        let mut c = crowd();
        for i in 0..40u64 {
            let p = NavLaneAgentParams {
                speed: 1.0 + (i % 5) as f32 * 0.2,
                ..Default::default()
            };
            let offset = (i % 3) as f32 * 0.5 - 0.5;
            c.add_agent(&graph, (i % 2) as u32, (i / 2) as f32 * 1.4, offset, p, i)
                .expect("valid agent");
        }
        let mut digests = Vec::new();
        for _ in 0..600 {
            c.step(&graph);
            digests.push(c.digest());
        }
        (digests, c.stats())
    };
    let (a, sa) = run();
    let (b, sb) = run();
    assert_eq!(a, b);
    assert_eq!(sa, sb);
    assert!(sa.oncoming_yields > 0);
}

#[test]
fn bad_weights_and_pairs_are_invalid_lanes() {
    let code = |defs: &[NavLaneDef]| NavLaneGraph::build(defs, 0.0).map(|_| ()).map_err(|e| e.code());
    let base = || vec![lane(&[(0.0, 0.0), (10.0, 0.0)]), lane(&[(10.0, 0.0), (0.0, 0.0)])];
    for w in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        let mut d = base();
        d[0].weight = w;
        assert_eq!(code(&d), Err("nav_invalid_lane"), "weight {w}");
    }
    let mut d = base();
    d[0].opposite = Some(0);
    assert_eq!(code(&d), Err("nav_invalid_lane"), "itself");
    let mut d = base();
    d[0].opposite = Some(5);
    assert_eq!(code(&d), Err("nav_invalid_lane"), "outside");
    // Not the same strip: the other lane runs elsewhere.
    let mut d = vec![lane(&[(0.0, 0.0), (10.0, 0.0)]), lane(&[(10.0, 9.0), (0.0, 9.0)])];
    d[0].opposite = Some(1);
    assert_eq!(code(&d), Err("nav_invalid_lane"), "elsewhere");
    // Same direction is not opposite.
    let mut d = vec![lane(&[(0.0, 0.0), (10.0, 0.0)]), lane(&[(0.0, 0.0), (10.0, 0.0)])];
    d[0].opposite = Some(1);
    assert_eq!(code(&d), Err("nav_invalid_lane"), "same way");
    // A three-way tangle.
    let mut d = base();
    d.push(lane(&[(10.0, 0.0), (0.0, 0.0)]));
    d[0].opposite = Some(1);
    d[1].opposite = Some(2);
    assert_eq!(code(&d), Err("nav_invalid_lane"), "tangle");
    // Both sides naming each other is fine.
    let mut d = base();
    d[0].opposite = Some(1);
    d[1].opposite = Some(0);
    assert_eq!(code(&d), Ok(()));
}

#[test]
fn a_v1_file_migrates_to_v2_and_v2_round_trips() {
    assert_eq!(NAV_LANES_FORMAT_VERSION, 2);
    assert_eq!(nav_lanes_migration_sources(), vec![1]);
    let v1 = "(format_version: 1, lanes: [(points: [(0.0, 0.0, 0.0), (4.0, 0.0, 0.0)])])";
    let old = NavLaneSet::parse(v1).expect("v1 loads");
    assert_eq!(old.lanes[0].weight, 1.0);
    let v2 = migrate_nav_lanes(v1).expect("migrates");
    assert_eq!(v2, v1.replace("format_version: 1", "format_version: 2"));
    let set = NavLaneSet::parse(&v2).expect("v2 loads");
    assert_eq!(set.lanes, old.lanes);

    let text = "(format_version: 2, lanes: [(points: [(0.0, 0.0, 0.0), (4.0, 0.0, 0.0)], weight: 2.5, \
                opposite: Some(1)), (points: [(4.0, 0.0, 0.0), (0.0, 0.0, 0.0)])])";
    let set = NavLaneSet::parse(text).expect("v2 loads");
    assert_eq!(set.lanes[0].weight, 2.5);
    assert_eq!(set.lanes[0].opposite, Some(1));
    let again = NavLaneSet::parse(&set.to_ron().expect("writes")).expect("re-reads");
    assert_eq!(again, set);
    assert_eq!(
        migrate_nav_lanes("(format_version: 3, lanes: [])").map_err(|e| e.code()),
        Err("nav_lane_version")
    );
}
