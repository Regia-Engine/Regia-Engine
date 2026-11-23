// SPDX-License-Identifier: MPL-2.0
//! Lanes from road splines (ADR-2420): `NavRoadNetwork` reads `regia_scene` splines and lays lanes
//! along them, within a stated tolerance of each lane's offset curve, two ways or one, joined end
//! to start, looped on closed splines.

use regia_core::math::{Affine3A, Quat, Vec3};
use regia_nav::{
    NavLaneAgentParams, NavLaneCrowd, NavLaneCrowdConfig, NavLaneSet, NavRoadLaneParams, NavRoadNetwork,
};
use regia_scene::spline::Spline;

/// The level right of a tangent, as the adapter defines it.
fn right(tangent: Vec3) -> Vec3 {
    tangent.cross(Vec3::Y).normalize()
}

fn dist_to_polyline(p: Vec3, line: &[(f32, f32, f32)]) -> f32 {
    line.windows(2)
        .map(|w| {
            let a = Vec3::new(w[0].0, w[0].1, w[0].2);
            let b = Vec3::new(w[1].0, w[1].1, w[1].2);
            let ab = b - a;
            let t = ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0);
            (a + ab * t - p).length()
        })
        .fold(f32::INFINITY, f32::min)
}

fn s_curve() -> Spline {
    Spline::through([
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(15.0, 1.0, 8.0),
        Vec3::new(30.0, 2.0, -8.0),
        Vec3::new(45.0, 2.0, 0.0),
    ])
}

#[test]
fn a_lane_follows_its_spline_within_the_tolerance() {
    let spline = s_curve();
    let pose = Affine3A::from_rotation_translation(Quat::from_rotation_y(0.7), Vec3::new(100.0, 3.0, -40.0));
    for max_error in [0.05f32, 0.01] {
        let params = NavRoadLaneParams {
            width: 8.0,
            lanes_each_way: 2,
            max_error,
            ..Default::default()
        };
        let mut net = NavRoadNetwork::new();
        let range = net.add_road(&spline, &pose, &params).expect("lanes");
        assert_eq!(range, 0..4);
        let (along, against) = params.offsets();
        assert_eq!(along, vec![1.0, 3.0]);
        assert_eq!(against, vec![-1.0, -3.0]);
        let curve = spline.world_curve(&pose);
        let length = curve.length();
        let offsets: Vec<f32> = along.iter().chain(&against).copied().collect();
        for (lane, &offset) in net.lanes().iter().zip(&offsets) {
            assert_eq!(lane.width, 2.0);
            // Every 2 cm of the exact offset curve lies within the tolerance of the lane.
            let mut d = 0.0;
            let mut worst = 0.0f32;
            while d <= length {
                let s = curve.at_distance(d);
                let exact = s.position + right(s.tangent) * offset;
                worst = worst.max(dist_to_polyline(exact, &lane.points));
                d += 0.02;
            }
            assert!(
                worst <= max_error + 1.0e-3,
                "offset {offset}: {worst} m off at tolerance {max_error}"
            );
        }
        // A tighter tolerance takes more points.
        if max_error < 0.05 {
            assert!(net.lanes()[0].points.len() > 8);
        }
    }
}

#[test]
fn a_straight_two_way_road_keeps_right() {
    let spline = Spline::through([Vec3::ZERO, Vec3::new(20.0, 0.0, 0.0)]);
    let mut net = NavRoadNetwork::new();
    let params = NavRoadLaneParams {
        first_user_id: 100,
        ..Default::default()
    };
    net.add_road(&spline, &Affine3A::IDENTITY, &params)
        .expect("lanes");
    let lanes = net.lanes();
    assert_eq!(lanes.len(), 2);
    // Along +X the right is +Z: that lane runs with the spline, the other one back.
    let (a, b) = (&lanes[0], &lanes[1]);
    assert!(
        a.points.iter().all(|p| (p.2 - 1.75).abs() < 1.0e-4),
        "{:?}",
        a.points
    );
    assert!(
        b.points.iter().all(|p| (p.2 + 1.75).abs() < 1.0e-4),
        "{:?}",
        b.points
    );
    assert!(a.points[0].0 < 1.0e-4 && b.points[0].0 > 19.999);
    assert_eq!((a.user_id, b.user_id), (100, 101));
    assert_eq!(
        (a.opposite, b.opposite),
        (None, None),
        "two strips, not one paired strip"
    );
    assert!(a.next.is_empty() && b.next.is_empty());
    let graph = net.into_set(0.0).expect("set").graph().expect("graph");
    assert!((graph.total_length() - 40.0).abs() < 1.0e-3);
}

#[test]
fn one_way_lanes_fill_the_carriageway_and_roads_join_end_to_start() {
    let params = NavRoadLaneParams {
        width: 6.0,
        lanes_each_way: 2,
        two_way: false,
        ..Default::default()
    };
    assert_eq!(params.offsets(), (vec![-1.5, 1.5], Vec::new()));
    let mut net = NavRoadNetwork::new();
    let first = Spline::through([Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0)]);
    let second = Spline::through([Vec3::new(10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0)]);
    assert_eq!(
        net.add_road(&first, &Affine3A::IDENTITY, &params).expect("a"),
        0..2
    );
    assert_eq!(
        net.add_road(&second, &Affine3A::IDENTITY, &params).expect("b"),
        2..4
    );
    let graph = net.into_set(0.25).expect("set").graph().expect("graph");
    assert_eq!(graph.lane(0).expect("lane").next, vec![2]);
    assert_eq!(graph.lane(1).expect("lane").next, vec![3]);
    assert!(graph.lane(2).expect("lane").next.is_empty());
    // A lane crowd walks from the first road onto the second.
    let mut crowd = NavLaneCrowd::new(NavLaneCrowdConfig::default()).expect("crowd");
    let id = crowd
        .add_agent(&graph, 0, 0.0, 0.0, NavLaneAgentParams::default(), 1)
        .expect("agent");
    let mut reached = false;
    for _ in 0..1200 {
        crowd.step(&graph);
        reached |= crowd.agent(id).is_some_and(|a| a.lane() == 2);
    }
    assert!(reached, "the agent took the joined lane");
}

#[test]
fn a_closed_spline_gives_looping_lanes() {
    let ring = Spline::through([
        Vec3::new(10.0, 0.0, 0.0),
        Vec3::new(0.0, 0.0, 10.0),
        Vec3::new(-10.0, 0.0, 0.0),
        Vec3::new(0.0, 0.0, -10.0),
    ])
    .closed();
    let mut net = NavRoadNetwork::new();
    net.add_road(&ring, &Affine3A::IDENTITY, &NavRoadLaneParams::default())
        .expect("lanes");
    for (i, lane) in net.lanes().iter().enumerate() {
        assert_eq!(lane.next, vec![i as u32], "lane {i} loops");
        let (first, last) = (lane.points[0], lane.points[lane.points.len() - 1]);
        let gap = Vec3::new(first.0 - last.0, first.1 - last.1, first.2 - last.2).length();
        assert!(gap < 1.0e-3, "lane {i} closes: {gap}");
    }
}

#[test]
fn the_set_round_trips_through_ron() {
    let mut net = NavRoadNetwork::new();
    net.add_road(&s_curve(), &Affine3A::IDENTITY, &NavRoadLaneParams::default())
        .expect("lanes");
    let set = net.into_set(1.0).expect("set");
    let text = set.to_ron().expect("writes");
    assert_eq!(NavLaneSet::parse(&text).expect("parses"), set);
}

#[test]
fn bad_roads_are_typed_errors() {
    let road = s_curve();
    let id = Affine3A::IDENTITY;
    let code = |params: NavRoadLaneParams| {
        NavRoadNetwork::new()
            .add_road(&road, &id, &params)
            .err()
            .map(|e| e.code())
    };
    let base = NavRoadLaneParams::default;
    assert_eq!(
        code(NavRoadLaneParams { width: 0.0, ..base() }),
        Some("nav_invalid_lane_config")
    );
    assert_eq!(
        code(NavRoadLaneParams {
            lanes_each_way: 0,
            ..base()
        }),
        Some("nav_invalid_lane_config")
    );
    assert_eq!(
        code(NavRoadLaneParams {
            lanes_each_way: 9,
            ..base()
        }),
        Some("nav_invalid_lane_config")
    );
    assert_eq!(
        code(NavRoadLaneParams {
            max_error: 0.0,
            ..base()
        }),
        Some("nav_invalid_lane_config")
    );
    let dot = Spline::through([Vec3::ZERO]);
    let mut net = NavRoadNetwork::new();
    assert!(net.add_road(&dot, &id, &base()).is_err());
    assert!(net.lanes().is_empty(), "a failure adds nothing");
}
