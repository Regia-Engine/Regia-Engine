// SPDX-License-Identifier: MPL-2.0
//! Degenerate inputs to the navigation API: non-finite geometry and parameters, empty and
//! off-mesh queries, odd extents, NaN agents for ORCA and empty paths. Nothing may panic, and a
//! nonsense input must give a miss (or a standstill), not a bogus path.

use regia_core::math::Vec3;
use regia_nav::orca::orca_velocity;
use regia_nav::{
    NavAgentState, NavBuildError, NavMesh, NavMeshBuildParams, NavMeshBuilder, NavObstacles, NavPath,
    NavPoly, NavQuery,
};

fn floor_triangles(half: f32) -> Vec<[Vec3; 3]> {
    vec![
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, -half),
            Vec3::new(half, 0.0, half),
        ],
        [
            Vec3::new(-half, 0.0, -half),
            Vec3::new(half, 0.0, half),
            Vec3::new(-half, 0.0, half),
        ],
    ]
}

fn floor_mesh() -> NavMesh {
    NavMeshBuilder::default()
        .build(&floor_triangles(6.0))
        .expect("a flat floor builds")
}

const EXTENTS: Vec3 = Vec3::new(2.0, 2.0, 2.0);

#[test]
fn builder_refuses_triangles_with_non_finite_vertices() {
    // A non-finite vertex is refused with a reason, never built around (the fix side's
    // `InvalidInput`, kept over ignoring the triangle, ADR-1363).
    let builder = NavMeshBuilder::default();
    let invalid = |r: Result<NavMesh, NavBuildError>| matches!(r, Err(NavBuildError::InvalidInput(_)));
    let nan = [[Vec3::NAN; 3]];
    assert!(invalid(builder.build(&nan)));
    let inf = [[
        Vec3::ZERO,
        Vec3::new(f32::INFINITY, 0.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
    ]];
    assert!(invalid(builder.build(&inf)));

    // Good triangles next to bad ones: the whole soup is refused, and the good ones alone build.
    let mut mixed = floor_triangles(6.0);
    mixed.push([Vec3::NAN; 3]);
    mixed.push([Vec3::ZERO, Vec3::splat(f32::NEG_INFINITY), Vec3::X]);
    assert!(invalid(builder.build(&mixed)));
    assert!(floor_mesh().poly_count() > 0);
    assert_eq!(builder.build(&[]).err(), Some(NavBuildError::NoGeometry));
}

#[test]
fn builder_rejects_unusable_voxel_sizes() {
    let triangles = floor_triangles(4.0);
    for (cell_size, cell_height) in [
        (0.0, 0.2),
        (-1.0, 0.2),
        (f32::NAN, 0.2),
        (f32::INFINITY, 0.2),
        (0.25, 0.0),
        (0.25, f32::NAN),
    ] {
        let builder = NavMeshBuilder::new(NavMeshBuildParams {
            cell_size,
            cell_height,
            ..NavMeshBuildParams::default()
        });
        assert!(
            matches!(builder.build(&triangles), Err(NavBuildError::InvalidInput(_))),
            "cell_size {cell_size}, cell_height {cell_height}"
        );
    }
    let builder = NavMeshBuilder::new(NavMeshBuildParams {
        border_size: f32::NAN,
        ..NavMeshBuildParams::default()
    });
    assert!(builder.build(&triangles).is_err());
}

#[test]
fn builder_survives_degenerate_triangles() {
    let builder = NavMeshBuilder::default();
    // Zero-area triangles (a point, a line) must not panic; whether they build is up to the
    // rasteriser, so only the absence of a panic is asserted.
    let _ = builder.build(&[[Vec3::ZERO; 3]]);
    let _ = builder.build(&[[Vec3::ZERO, Vec3::X, Vec3::new(2.0, 0.0, 0.0)]]);
}

#[test]
fn queries_with_non_finite_points_miss() {
    let mesh = floor_mesh();
    let obstacles = NavObstacles::default();
    let query = NavQuery {
        mesh: &mesh,
        obstacles: Some(&obstacles),
    };
    assert!(query.find_nearest(Vec3::ZERO, EXTENTS).is_some());
    for bad in [
        Vec3::NAN,
        Vec3::new(f32::NAN, 0.0, 0.0),
        Vec3::new(f32::INFINITY, 0.0, 0.0),
        Vec3::new(0.0, 0.0, f32::NEG_INFINITY),
        Vec3::new(1.0e30, 0.0, 0.0),
        Vec3::new(0.0, 0.0, -1.0e30),
    ] {
        assert!(query.find_nearest(bad, EXTENTS).is_none(), "{bad:?}");
        assert!(query.find_path(bad, Vec3::ZERO, EXTENTS).is_none(), "{bad:?}");
        assert!(query.find_path(Vec3::ZERO, bad, EXTENTS).is_none(), "{bad:?}");
    }
}

#[test]
fn queries_with_odd_extents() {
    let mesh = floor_mesh();
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    // A point well off the mesh: found only when the extents reach it.
    let off = Vec3::new(10.0, 0.0, 0.0);
    assert!(query.find_nearest(off, EXTENTS).is_none());
    assert!(query.find_nearest(off, Vec3::new(8.0, 2.0, 8.0)).is_some());
    // A point high above the floor needs the vertical extent to reach it.
    let high = Vec3::new(0.0, 50.0, 0.0);
    assert!(query.find_nearest(high, EXTENTS).is_none());
    assert!(query.find_nearest(high, Vec3::new(2.0, 60.0, 2.0)).is_some());
    // Negative or NaN vertical extents never match; neither do NaN horizontal ones.
    assert!(
        query
            .find_nearest(Vec3::ZERO, Vec3::new(2.0, -1.0, 2.0))
            .is_none()
    );
    assert!(
        query
            .find_nearest(Vec3::ZERO, Vec3::new(2.0, f32::NAN, 2.0))
            .is_none()
    );
    assert!(
        query
            .find_nearest(Vec3::ZERO, Vec3::new(f32::NAN, 2.0, 2.0))
            .is_none()
    );
}

#[test]
fn paths_between_the_same_point_and_off_mesh_ends() {
    let mesh = floor_mesh();
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let here = Vec3::new(1.0, 0.0, 1.0);
    let path = query.find_path(here, here, EXTENTS).expect("a zero-length path");
    assert!(path.length.abs() < 1e-5);
    assert!(path.waypoints.len() >= 2);
    assert!(!path.polys.is_empty());

    let far = Vec3::new(500.0, 0.0, 500.0);
    assert!(query.find_path(here, far, EXTENTS).is_none());
    assert!(query.find_path(far, here, EXTENTS).is_none());
    // The mesh sits at y = 0: a start ten metres up misses with small extents.
    assert!(
        query
            .find_path(Vec3::new(0.0, 10.0, 0.0), here, EXTENTS)
            .is_none()
    );
}

#[test]
fn a_path_is_never_shorter_than_the_straight_line() {
    let mesh = floor_mesh();
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let (a, b) = (Vec3::new(-4.0, 0.0, -4.0), Vec3::new(4.0, 0.0, 3.0));
    let path = query.find_path(a, b, EXTENTS).expect("path");
    assert!(path.length + 1e-3 >= (b - a).length());
    assert!(path.waypoints.iter().all(|w| w.is_finite()));
    let measured: f32 = path.waypoints.windows(2).map(|p| (p[1] - p[0]).length()).sum();
    assert!((measured - path.length).abs() < 1e-3);
}

#[test]
fn obstacle_set_edge_cases() {
    let mut obstacles = NavObstacles::default();
    assert_eq!(obstacles.obstacle_count(), 0);
    assert!(!obstacles.remove(0));
    let a = obstacles.add_box(Vec3::splat(-1.0), Vec3::splat(1.0));
    let b = obstacles.add_box(Vec3::splat(-1.0), Vec3::splat(1.0));
    assert_ne!(a, b);
    assert_eq!(obstacles.obstacle_count(), 2);
    assert!(obstacles.remove(a));
    assert!(!obstacles.remove(a), "already removed");
    assert_eq!(obstacles.obstacle_count(), 1);
    assert!(obstacles.remove(b));
    assert_eq!(obstacles.obstacle_count(), 0);
    // Ids are never reused after a removal.
    let c = obstacles.add_box(Vec3::ZERO, Vec3::ONE);
    assert!(c != a && c != b);
}

#[test]
fn nan_and_inverted_obstacle_boxes_carve_nothing() {
    let mesh = floor_mesh();
    let mut obstacles = NavObstacles::default();
    obstacles.add_box(Vec3::NAN, Vec3::NAN);
    // Inverted: min above max on every axis.
    obstacles.add_box(Vec3::splat(2.0), Vec3::splat(-2.0));
    let query = NavQuery {
        mesh: &mesh,
        obstacles: Some(&obstacles),
    };
    let a = Vec3::new(-4.0, 0.0, 0.0);
    let b = Vec3::new(4.0, 0.0, 0.0);
    let path = query.find_path(a, b, EXTENTS).expect("the floor is still open");
    assert!(path.length.is_finite());
}

#[test]
fn an_obstacle_covering_the_whole_floor_leaves_no_path() {
    let mesh = floor_mesh();
    let mut obstacles = NavObstacles::default();
    obstacles.add_box(Vec3::new(-100.0, -10.0, -100.0), Vec3::new(100.0, 10.0, 100.0));
    let query = NavQuery {
        mesh: &mesh,
        obstacles: Some(&obstacles),
    };
    assert!(query.find_nearest(Vec3::ZERO, EXTENTS).is_none());
    assert!(
        query
            .find_path(Vec3::new(-3.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0), EXTENTS)
            .is_none()
    );
}

#[test]
fn a_polygon_without_vertices_contains_nothing() {
    let poly = NavPoly {
        vertices: Vec::new(),
        area: 0,
        portals: Vec::new(),
        min: Vec3::ZERO,
        max: Vec3::ZERO,
    };
    assert!(!poly.contains_xz(0.0, 0.0));
    assert_eq!(poly.distance_sq_xz(1.0, 1.0), f32::INFINITY);
    assert_eq!(poly.height(), 0.0);
    assert_eq!(poly.center_xz(), Vec3::ZERO);
}

#[test]
fn polys_in_box_handles_empty_and_inverted_boxes() {
    let mesh = floor_mesh();
    assert!(
        !mesh
            .polys_in_box(Vec3::splat(-100.0), Vec3::splat(100.0))
            .is_empty()
    );
    assert!(
        mesh.polys_in_box(Vec3::splat(100.0), Vec3::splat(200.0))
            .is_empty()
    );
    // Inverted bounds match nothing, and NaN bounds match nothing.
    assert!(
        mesh.polys_in_box(Vec3::splat(100.0), Vec3::splat(-100.0))
            .is_empty()
    );
    assert!(mesh.polys_in_box(Vec3::NAN, Vec3::NAN).is_empty());
}

#[test]
fn agent_state_with_empty_and_single_point_paths() {
    let mut state = NavAgentState::default();
    assert!(!state.has_path());
    assert_eq!(state.current_waypoint(), None);
    state.set_path(NavPath {
        polys: Vec::new(),
        waypoints: Vec::new(),
        length: 0.0,
    });
    assert!(!state.has_path());
    assert_eq!(state.target, None);
    assert_eq!(state.current_waypoint(), None);

    // One waypoint is the agent's own position: nothing left to follow.
    state.set_path(NavPath {
        polys: vec![0],
        waypoints: vec![Vec3::ONE],
        length: 0.0,
    });
    assert!(!state.has_path());
    assert_eq!(state.target, Some(Vec3::ONE));
    state.clear_path();
    assert_eq!(state.target, None);
    state.clear_path();
    assert_eq!(state.next_waypoint, 0);
}

#[test]
fn orca_with_non_finite_inputs_stands_still() {
    let ok = |v: Vec3| v.is_finite();
    let n = [(Vec3::new(2.0, 0.0, 0.0), Vec3::ZERO, 0.5)];
    let go = Vec3::X;
    assert_eq!(
        orca_velocity(Vec3::NAN, Vec3::ZERO, go, 0.5, 3.0, 1.0, &n),
        Vec3::ZERO
    );
    assert_eq!(
        orca_velocity(Vec3::ZERO, Vec3::NAN, go, 0.5, 3.0, 1.0, &n),
        Vec3::ZERO
    );
    assert_eq!(
        orca_velocity(Vec3::ZERO, Vec3::ZERO, Vec3::NAN, 0.5, 3.0, 1.0, &n),
        Vec3::ZERO
    );
    assert_eq!(
        orca_velocity(Vec3::ZERO, Vec3::ZERO, go, f32::NAN, 3.0, 1.0, &n),
        Vec3::ZERO
    );
    assert_eq!(
        orca_velocity(Vec3::ZERO, Vec3::ZERO, go, 0.5, f32::NAN, 1.0, &n),
        Vec3::ZERO
    );
    assert_eq!(
        orca_velocity(Vec3::ZERO, Vec3::ZERO, go, 0.5, 3.0, f32::NAN, &n),
        Vec3::ZERO
    );

    // A NaN neighbour is ignored: the lone agent keeps its preferred velocity.
    let bad = [
        (Vec3::NAN, Vec3::ZERO, 0.5),
        (Vec3::X, Vec3::NAN, 0.5),
        (Vec3::X, Vec3::ZERO, f32::NAN),
    ];
    let v = orca_velocity(Vec3::ZERO, Vec3::ZERO, go, 0.5, 3.0, 1.0, &bad);
    assert!(ok(v));
    assert!((v - go).length() < 1e-3, "{v:?}");
}

#[test]
fn orca_degenerate_limits() {
    let go = Vec3::new(5.0, 0.0, 0.0);
    // No speed budget: no motion.
    let v = orca_velocity(Vec3::ZERO, Vec3::ZERO, go, 0.5, 0.0, 1.0, &[]);
    assert!(v.length() < 1e-6, "{v:?}");
    // A negative speed limit is treated as zero.
    let v = orca_velocity(Vec3::ZERO, Vec3::ZERO, go, 0.5, -3.0, 1.0, &[]);
    assert!(v.length() < 1e-6, "{v:?}");
    // Zero and negative horizons and radii stay finite.
    for (radius, horizon) in [(0.0, 1.0), (0.5, 0.0), (0.5, -2.0), (-0.5, 1.0), (0.0, 0.0)] {
        let v = orca_velocity(
            Vec3::ZERO,
            Vec3::ZERO,
            go,
            radius,
            3.0,
            horizon,
            &[(Vec3::new(1.0, 0.0, 0.0), Vec3::new(-1.0, 0.0, 0.0), 0.5)],
        );
        assert!(v.is_finite(), "radius {radius}, horizon {horizon}: {v:?}");
        assert!(v.length() <= 3.0 + 1e-3);
    }
}

#[test]
fn orca_crowd_of_co_located_agents_stays_finite_and_bounded() {
    let neighbors: Vec<(Vec3, Vec3, f32)> = (0..16).map(|_| (Vec3::ZERO, Vec3::ZERO, 0.5)).collect();
    let v = orca_velocity(Vec3::ZERO, Vec3::ZERO, Vec3::X, 0.5, 3.0, 1.0, &neighbors);
    assert!(v.is_finite());
    assert!(v.length() <= 3.0 + 1e-3);
    assert_eq!(v.y, 0.0, "avoidance is planar");
}

#[test]
fn orca_ignores_the_vertical_axis() {
    let v = orca_velocity(
        Vec3::new(0.0, 100.0, 0.0),
        Vec3::new(0.0, 7.0, 0.0),
        Vec3::new(1.0, 9.0, 0.0),
        0.5,
        3.0,
        1.0,
        &[(Vec3::new(50.0, -100.0, 50.0), Vec3::new(0.0, 3.0, 0.0), 0.5)],
    );
    assert_eq!(v.y, 0.0);
    assert!((v.x - 1.0).abs() < 1e-3);
}

#[test]
fn shift_origin_moves_polygons_in_y_but_contours_only_in_xz() {
    // `contours` are display outlines at y = 0: the shift moves them in X and Z, never in Y.
    let mut mesh = floor_mesh();
    assert!(
        mesh.contours.iter().any(|c| !c.loops.is_empty()),
        "the floor has a contour"
    );
    let before: Vec<Vec3> = mesh
        .contours
        .iter()
        .flat_map(|c| c.loops.iter().flatten().copied())
        .collect();
    let poly_y = mesh.polys[0].vertices[0].y;
    mesh.shift_origin(Vec3::new(3.0, 4.0, -2.0));
    let after: Vec<Vec3> = mesh
        .contours
        .iter()
        .flat_map(|c| c.loops.iter().flatten().copied())
        .collect();
    assert_eq!(before.len(), after.len());
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(a.x, b.x + 3.0);
        assert_eq!(a.z, b.z - 2.0);
        assert_eq!(a.y, b.y, "contour y stays on its plane");
    }
    assert_eq!(
        mesh.polys[0].vertices[0].y,
        poly_y + 4.0,
        "polygons move by the full delta"
    );
}
