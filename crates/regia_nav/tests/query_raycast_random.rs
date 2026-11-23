// SPDX-License-Identifier: MPL-2.0
//! NM-1 navmesh queries: a raycast along the mesh, random points (whole mesh and in a radius) and
//! partial paths to an unreachable goal. Every fixture is built with a zero build radius and a
//! 0.5 m cell, so the walkable edges sit exactly on the source rectangles and each expectation is
//! analytic. Seeded draws only; nothing reads the wall clock.

use regia_core::math::{Rng, Vec3};
use regia_nav::{
    NavAreaVolume, NavMesh, NavMeshBuildParams, NavMeshBuilder, NavObstacleShape, NavPathResult, NavQuery,
    NavQueryError, NavQueryFilter,
};

const REACH: Vec3 = Vec3::splat(1.0);
const MUD: u8 = 3;

fn params() -> NavMeshBuildParams {
    let mut p = NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    p.agent.radius = 0.0;
    p
}

fn quad(x0: f32, x1: f32, z0: f32, z1: f32) -> Vec<[Vec3; 3]> {
    vec![
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

fn build(quads: &[[f32; 4]], volumes: &[NavAreaVolume]) -> NavMesh {
    let tris: Vec<[Vec3; 3]> = quads.iter().flat_map(|q| quad(q[0], q[1], q[2], q[3])).collect();
    NavMeshBuilder::new(params())
        .build_with_areas(&tris, volumes)
        .expect("the fixture builds")
}

fn query(mesh: &NavMesh) -> NavQuery<'_> {
    NavQuery {
        mesh,
        obstacles: None,
    }
}

fn assert_close(got: f32, want: f32, what: &str) {
    assert!((got - want).abs() < 1e-4, "{what}: got {got}, want {want}");
}

fn assert_normal(got: Vec3, want: Vec3) {
    assert!((got - want).length() < 1e-4, "normal: got {got:?}, want {want:?}");
}

/// Two 10 x 12 m rooms joined by a 4 m long, 2 m wide door at z in [-1, 1].
fn two_rooms() -> NavMesh {
    build(
        &[
            [-12.0, -2.0, -6.0, 6.0],
            [-2.0, 2.0, -1.0, 1.0],
            [2.0, 12.0, -6.0, 6.0],
        ],
        &[],
    )
}

#[test]
fn raycast_stops_at_a_wall_and_passes_an_open_room() {
    let room = build(&[[-8.0, 8.0, -8.0, 8.0]], &[]);
    let q = query(&room);
    let all = NavQueryFilter::default();

    // East wall at x = 8: t = 8 / 20, the normal points back west.
    let east = q
        .raycast(Vec3::ZERO, Vec3::new(20.0, 0.0, 0.0), REACH, &all)
        .expect("starts on the mesh");
    assert!(east.hit);
    assert_close(east.t, 0.4, "east t");
    assert_normal(east.normal, Vec3::new(-1.0, 0.0, 0.0));
    assert_close(east.point.x, 8.0, "east point x");
    assert_close(east.point.z, 0.0, "east point z");
    assert_eq!(east.path_polys.last(), Some(&east.last_poly));

    // North wall at z = 8 on a slanted ray: t = 8 / 16, the hit at x = 2.
    let north = q
        .raycast(Vec3::ZERO, Vec3::new(4.0, 0.0, 16.0), REACH, &all)
        .expect("starts on the mesh");
    assert!(north.hit);
    assert_close(north.t, 0.5, "north t");
    assert_normal(north.normal, Vec3::new(0.0, 0.0, -1.0));
    assert_close(north.point.x, 2.0, "north point x");

    // Inside the room: nothing in the way.
    let open = q
        .raycast(Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, 4.0), REACH, &all)
        .expect("starts on the mesh");
    assert!(!open.hit);
    assert_eq!(open.t, 1.0);
    assert_eq!(open.normal, Vec3::ZERO);
    assert!(room.polys[open.last_poly as usize].distance_sq_xz(5.0, 4.0) < 1e-6);

    // Through the door of two rooms: clear, across at least the three pieces.
    let rooms = two_rooms();
    let q = query(&rooms);
    let through = q
        .raycast(Vec3::new(-8.0, 0.0, 0.0), Vec3::new(8.0, 0.0, 0.0), REACH, &all)
        .expect("starts on the mesh");
    assert!(!through.hit, "{through:?}");
    assert!(through.path_polys.len() >= 3, "{:?}", through.path_polys);
    assert!(rooms.polys[through.last_poly as usize].distance_sq_xz(8.0, 0.0) < 1e-6);
    // Beside the door: room A's east wall at x = -2, t = 6 / 16.
    let beside = q
        .raycast(Vec3::new(-8.0, 0.0, 3.0), Vec3::new(8.0, 0.0, 3.0), REACH, &all)
        .expect("starts on the mesh");
    assert!(beside.hit);
    assert_close(beside.t, 0.375, "beside t");
    assert_normal(beside.normal, Vec3::new(-1.0, 0.0, 0.0));
    // Into the door and out through its north side (z = 1) at x = 0: t = 4 / 8.
    let side = q
        .raycast(Vec3::new(-4.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 2.0), REACH, &all)
        .expect("starts on the mesh");
    assert!(side.hit);
    assert_close(side.t, 0.5, "door side t");
    assert_normal(side.normal, Vec3::new(0.0, 0.0, -1.0));
    assert_close(side.point.x, 0.0, "door side x");
    assert_close(side.point.z, 1.0, "door side z");
    // The walk is the crossed polygons in order: each consecutive pair shares a portal.
    for pair in side.path_polys.windows(2) {
        let a = &rooms.polys[pair[0] as usize];
        assert!(
            a.portals.iter().any(|p| p.neighbor == pair[1]),
            "{:?}",
            side.path_polys
        );
    }

    // Typed errors.
    assert_eq!(
        q.raycast(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, REACH, &all),
        Err(NavQueryError::NoStartPoly)
    );
    assert_eq!(
        q.raycast(Vec3::ZERO, Vec3::X, Vec3::new(-1.0, 1.0, 1.0), &all),
        Err(NavQueryError::BadExtents)
    );
    assert_eq!(NavQueryError::NoStartPoly.code(), "nav_no_start_poly");
    assert_eq!(NavQueryError::BadExtents.code(), "nav_bad_extents");
}

fn mud_strip() -> NavAreaVolume {
    NavAreaVolume::new(
        NavObstacleShape::Box {
            min: Vec3::new(-2.0, -1.0, -16.5),
            max: Vec3::new(2.0, 1.0, 16.5),
        },
        MUD,
    )
}

#[test]
fn raycast_respects_area_filters() {
    let mesh = build(&[[-16.0, 16.0, -16.0, 16.0]], &[mud_strip()]);
    let q = query(&mesh);
    let start = Vec3::new(-8.0, 0.0, 0.0);
    let end = Vec3::new(8.0, 0.0, 0.0);

    let anyone = q
        .raycast(start, end, REACH, &NavQueryFilter::default())
        .expect("on the mesh");
    assert!(!anyone.hit, "mud is walkable by default");
    assert!(
        anyone
            .path_polys
            .iter()
            .any(|&p| mesh.polys[p as usize].area == MUD)
    );

    let no_mud = NavQueryFilter::default().without_area(MUD).expect("valid id");
    let stopped = q.raycast(start, end, REACH, &no_mud).expect("on the mesh");
    assert!(stopped.hit, "the left-out strip is a wall");
    assert_close(stopped.t, 6.0 / 16.0, "mud edge t");
    assert_normal(stopped.normal, Vec3::new(-1.0, 0.0, 0.0));
    assert!(
        stopped
            .path_polys
            .iter()
            .all(|&p| mesh.polys[p as usize].area != MUD),
        "no polygon of the walk is mud"
    );
}

/// Draws `n` points and counts them per polygon; asserts each lies on its polygon.
fn draw_counts(mesh: &NavMesh, filter: &NavQueryFilter, seed: u64, n: usize) -> (Vec<usize>, Vec<Vec3>) {
    let q = query(mesh);
    let mut rng = Rng::new(seed);
    let mut counts = vec![0usize; mesh.polys.len()];
    let mut points = Vec::with_capacity(n);
    for _ in 0..n {
        let (poly, p) = q.random_point(filter, &mut rng).expect("the mesh has area");
        let pg = &mesh.polys[poly as usize];
        assert!(
            pg.distance_sq_xz(p.x, p.z) < 1e-6,
            "{p:?} is not on polygon {poly}"
        );
        assert_eq!(p.y, pg.height());
        counts[poly as usize] += 1;
        points.push(p);
    }
    (counts, points)
}

fn xz_area(vertices: &[Vec3]) -> f32 {
    let n = vertices.len();
    let mut s = 0.0;
    for i in 0..n {
        let (a, b) = (vertices[i], vertices[(i + 1) % n]);
        s += a.x * b.z - b.x * a.z;
    }
    s.abs() * 0.5
}

#[test]
fn random_points_are_on_the_mesh_and_area_weighted() {
    let mesh = two_rooms();
    let all = NavQueryFilter::default();
    let n = 10_000;
    let (counts, points) = draw_counts(&mesh, &all, 42, n);
    let areas: Vec<f32> = mesh.polys.iter().map(|p| xz_area(&p.vertices)).collect();
    let total: f32 = areas.iter().sum();
    assert_close(total, 2.0 * 120.0 + 8.0, "fixture area");
    for (i, (&c, &a)) in counts.iter().zip(&areas).enumerate() {
        let p = f64::from(a / total);
        let expected = p * n as f64;
        // Five binomial standard deviations plus one count of slack.
        let bound = 5.0 * (expected * (1.0 - p)).sqrt() + 1.0;
        assert!(
            (c as f64 - expected).abs() <= bound,
            "polygon {i}: {c} draws, expected {expected:.1} ± {bound:.1}"
        );
    }
    // The halves of the mesh split by x = 0 hold about half the draws each (the door is
    // symmetric about it).
    let west = points.iter().filter(|p| p.x < 0.0).count() as f64;
    assert!((west - 5000.0).abs() < 5.0 * 50.0, "west draws {west}");

    // Same seed, same points; another seed, other points.
    let (_, again) = draw_counts(&mesh, &all, 42, n);
    assert_eq!(points, again);
    let (_, other) = draw_counts(&mesh, &all, 43, 16);
    assert_ne!(points[..16], other[..]);

    // A left-out area gets no points.
    let mud = build(&[[-16.0, 16.0, -16.0, 16.0]], &[mud_strip()]);
    let no_mud = NavQueryFilter::default().without_area(MUD).expect("valid id");
    let (counts, _) = draw_counts(&mud, &no_mud, 7, 2000);
    for (i, &c) in counts.iter().enumerate() {
        if mud.polys[i].area == MUD {
            assert_eq!(c, 0, "polygon {i} is mud");
        }
    }
    // Everything left out: no point.
    let nothing = NavQueryFilter::default()
        .without_area(0)
        .and_then(|f| f.without_area(MUD))
        .expect("valid ids");
    assert_eq!(query(&mud).random_point(&nothing, &mut Rng::new(1)), None);
}

#[test]
fn random_point_in_radius_reachable_only_skips_islands() {
    // The main floor x in [-8, 0]; an island x in [1, 5], 1 m away with no link.
    let mesh = build(&[[-8.0, 0.0, -4.0, 4.0], [1.0, 5.0, -4.0, 4.0]], &[]);
    let q = query(&mesh);
    let all = NavQueryFilter::default();
    let center = Vec3::new(-1.0, 0.0, 0.0);
    let radius = 4.0;

    let mut rng = Rng::new(9);
    let mut on_island = 0;
    for _ in 0..2000 {
        let (poly, p) = q
            .random_point_in_radius(center, radius, REACH, true, &all, &mut rng)
            .expect("valid query")
            .expect("the circle touches the floor");
        assert!(mesh.polys[poly as usize].distance_sq_xz(p.x, p.z) < 1e-6);
        assert!((p - center).length() <= radius + 1e-4, "{p:?} outside the circle");
        if p.x > 0.5 {
            on_island += 1;
        }
    }
    assert_eq!(on_island, 0, "reachable_only never picks the island");

    // Without reachable_only the island is in play, about in proportion to its area in the
    // circle (it holds a good share: well over 100 of 2000).
    let mut rng = Rng::new(9);
    let mut on_island = 0;
    for _ in 0..2000 {
        let (_, p) = q
            .random_point_in_radius(center, radius, REACH, false, &all, &mut rng)
            .expect("valid query")
            .expect("the circle touches the floor");
        assert!((p - center).length() <= radius + 1e-4, "{p:?} outside the circle");
        if p.x > 0.5 {
            on_island += 1;
        }
    }
    assert!(on_island > 100, "only {on_island} island points");

    // Replayable from the seed.
    let a = q.random_point_in_radius(center, radius, REACH, true, &all, &mut Rng::new(5));
    let b = q.random_point_in_radius(center, radius, REACH, true, &all, &mut Rng::new(5));
    assert_eq!(a, b);

    // Typed errors and an empty circle.
    assert_eq!(
        q.random_point_in_radius(center, -1.0, REACH, true, &all, &mut Rng::new(1)),
        Err(NavQueryError::BadRadius)
    );
    assert_eq!(
        q.random_point_in_radius(
            Vec3::new(40.0, 0.0, 0.0),
            2.0,
            REACH,
            true,
            &all,
            &mut Rng::new(1)
        ),
        Err(NavQueryError::NoStartPoly)
    );
    assert_eq!(
        q.random_point_in_radius(
            Vec3::new(40.0, 0.0, 0.0),
            2.0,
            REACH,
            false,
            &all,
            &mut Rng::new(1)
        ),
        Ok(None)
    );
}

#[test]
fn unreachable_goal_gives_the_nearest_partial_path() {
    let mesh = build(&[[-8.0, 0.0, -4.0, 4.0], [1.0, 5.0, -4.0, 4.0]], &[]);
    let q = query(&mesh);
    let all = NavQueryFilter::default();
    let start = Vec3::new(-6.0, 0.0, 0.0);
    let goal = Vec3::new(3.0, 0.0, 0.0);

    // find_path keeps its answer: no route.
    assert!(q.find_path(start, goal, REACH).is_none());

    let NavPathResult::Partial(path) = q.find_path_partial(start, goal, REACH, &all) else {
        panic!("an unreachable goal gives a partial path");
    };
    let last = *path.waypoints.last().expect("waypoints");
    assert_close(last.x, 0.0, "partial end x");
    assert_close(last.z, 0.0, "partial end z");
    assert_close(path.length, 6.0, "partial length");
    assert!(path.polys.iter().all(|&p| mesh.polys[p as usize].max.x <= 1e-4));
    // The nearest polygon, ties by the lower index.
    let dist = |i: usize| mesh.polys[i].distance_sq_xz(goal.x, goal.z);
    let main: Vec<usize> = (0..mesh.polys.len())
        .filter(|&i| mesh.polys[i].max.x <= 1e-4)
        .collect();
    let best = main.iter().map(|&i| dist(i)).fold(f32::INFINITY, f32::min);
    let want = main
        .iter()
        .copied()
        .find(|&i| dist(i) == best)
        .expect("a main polygon");
    assert_eq!(path.polys.last().copied(), Some(want as u32));

    // A goal off the mesh is unreachable too: the path ends at the nearest edge point.
    let NavPathResult::Partial(off) = q.find_path_partial(start, Vec3::new(30.0, 0.0, 2.0), REACH, &all)
    else {
        panic!("a goal off the mesh gives a partial path");
    };
    let end = *off.waypoints.last().expect("waypoints");
    // (30, 2) is nearest the island, which the start cannot reach: the main floor's edge.
    assert_close(end.x, 0.0, "off-mesh partial end x");
    assert_close(end.z, 2.0, "off-mesh partial end z");
    assert!(off.polys.iter().all(|&p| mesh.polys[p as usize].max.x <= 1e-4));

    // A reachable goal gives exactly find_path's path, as Complete.
    let reach = Vec3::new(-1.0, 0.0, 3.0);
    let full = q.find_path(start, reach, REACH).expect("reachable");
    assert_eq!(
        q.find_path_partial(start, reach, REACH, &all),
        NavPathResult::Complete(full)
    );
    // A start off the mesh.
    assert_eq!(
        q.find_path_partial(Vec3::new(0.5, 0.0, 9.0), goal, REACH, &all),
        NavPathResult::NoStart
    );
}
