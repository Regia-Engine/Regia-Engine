// SPDX-License-Identifier: MPL-2.0
//! Continuous shortest-path checks for flat simple polygon strips (spec 22 T2.2).
//!
//! This reference does not orient portals, tighten a funnel, or price polygon edges. A shortest
//! path in a simple polygon bends only at boundary vertices: build the small dense visibility
//! graph and solve it with Dijkstra (Mount, CMSC 754, Lecture 26; docs/provenance.md). Extra
//! subdivision vertices do not change that optimum. Every fixture's polygon dual is a chain,
//! so these checks make no claim about choosing among different A* corridors.

use regia_core::math::Vec3;
use regia_nav::{NavMesh, NavMeshBuildParams, NavMeshBuilder, NavPath, NavPoly, NavQuery, Portal};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    x: f64,
    z: f64,
}

impl Point {
    fn from_vec3(v: Vec3) -> Self {
        Self {
            x: f64::from(v.x),
            z: f64::from(v.z),
        }
    }

    fn vec3(self) -> Vec3 {
        Vec3::new(self.x as f32, 0.0, self.z as f32)
    }

    fn distance(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.z - other.z)
    }
}

fn point(x: i32, z: i32) -> Point {
    Point {
        x: f64::from(x),
        z: f64::from(z),
    }
}

fn polygon(coords: &[(i32, i32)]) -> Vec<Point> {
    coords.iter().map(|&(x, z)| point(x, z)).collect()
}

fn rectangle(x0: i32, z0: i32, x1: i32, z1: i32) -> Vec<Point> {
    polygon(&[(x0, z0), (x1, z0), (x1, z1), (x0, z1)])
}

fn cross(ax: f64, az: f64, bx: f64, bz: f64) -> f64 {
    ax * bz - az * bx
}

fn edges(poly: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    poly.iter()
        .copied()
        .zip(poly.iter().copied().cycle().skip(1))
        .take(poly.len())
}

// Fixtures use small integers (|coordinate| <= 128): their differences, products and determinant
// signs are exact in f64. No geometric epsilon changes a half-plane's inside/outside decision.
// Only division to obtain t and joining intervals round: 64 f64 ulps of [0,1] cover that error
// (at most six constraints per polygon), independently of the engine's f32 funnel predicates.
const PARAMETER_ROUNDOFF: f64 = 64.0 * f64::EPSILON;

fn clipped_interval(poly: &[Point], a: Point, b: Point) -> Option<(f64, f64)> {
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    for (v, w) in edges(poly) {
        // For a CCW convex polygon: cross(w-v, a-v + t*(b-a)) >= 0.
        let at_start = cross(w.x - v.x, w.z - v.z, a.x - v.x, a.z - v.z);
        let slope = cross(w.x - v.x, w.z - v.z, b.x - a.x, b.z - a.z);
        if slope > 0.0 {
            lo = lo.max(-at_start / slope);
        } else if slope < 0.0 {
            hi = hi.min(-at_start / slope);
        } else if at_start < 0.0 {
            return None;
        }
    }
    (lo <= hi).then_some((lo, hi))
}

// Cover the ENTIRE closed segment with the union of convex clipping intervals. Endpoints,
// collinear boundary travel, and a vertex touch count; an excursion between samples cannot hide.
fn visible(polys: &[Vec<Point>], a: Point, b: Point) -> bool {
    let mut intervals: Vec<_> = polys.iter().filter_map(|p| clipped_interval(p, a, b)).collect();
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut covered = 0.0_f64;
    for (lo, hi) in intervals {
        if lo > covered + PARAMETER_ROUNDOFF {
            return false;
        }
        covered = covered.max(hi);
        if covered >= 1.0 - PARAMETER_ROUNDOFF {
            return true;
        }
    }
    false
}

fn polyline_length(points: &[Point]) -> f64 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum()
}

fn shortest_path(polys: &[Vec<Point>], start: Point, end: Point) -> Vec<Point> {
    assert_ne!(start, end);
    let mut nodes = vec![start, end];
    for &v in polys.iter().flatten() {
        if !nodes.contains(&v) {
            nodes.push(v);
        }
    }
    let n = nodes.len();
    let mut weights = vec![vec![f64::INFINITY; n]; n];
    for (i, &a) in nodes.iter().enumerate() {
        for (j, &b) in nodes.iter().enumerate().skip(i + 1) {
            if visible(polys, a, b) {
                let d = a.distance(b);
                weights[i][j] = d;
                weights[j][i] = d;
            }
        }
    }
    let mut distance = vec![f64::INFINITY; n];
    let mut settled = vec![false; n];
    let mut parent = vec![None; n];
    distance[0] = 0.0;
    for _ in 0..n {
        let u = (0..n)
            .filter(|&i| !settled[i])
            .min_by(|&i, &j| distance[i].total_cmp(&distance[j]))
            .expect("an unsettled node");
        assert!(distance[u].is_finite(), "fixture visibility graph disconnected");
        if u == 1 {
            let mut path = vec![end];
            let mut cur = 1;
            while cur != 0 {
                cur = parent[cur].expect("reachable node has a predecessor");
                path.push(nodes[cur]);
                assert!(path.len() <= n);
            }
            path.reverse();
            return path;
        }
        settled[u] = true;
        for v in 0..n {
            let candidate = distance[u] + weights[u][v];
            if !settled[v] && candidate < distance[v] {
                distance[v] = candidate;
                parent[v] = Some(u);
            }
        }
    }
    panic!("fixture has no visibility path");
}

// NavMesh has no public constructor from polygons. Use the public builder to allocate N indexed
// polygon slots, then replace its public polys with the authored geometry. The seed islands are
// NOT queried: REACH covers every seed bucket, so the private index returns all N polygon ids.
// The final fixture has no holes/islands, and exact endpoint checks below forbid a hidden snap.
const REACH: Vec3 = Vec3::splat(1024.0);

fn mesh_from_polygons(polys: &[Vec<Point>]) -> NavMesh {
    let mut triangles = Vec::new();
    for i in 0..polys.len() {
        let p = rectangle(4 * i as i32, 0, 4 * i as i32 + 2, 2);
        triangles.push([p[0].vec3(), p[1].vec3(), p[2].vec3()]);
        triangles.push([p[0].vec3(), p[2].vec3(), p[3].vec3()]);
    }
    let mut params = NavMeshBuildParams {
        cell_size: 0.5,
        ..Default::default()
    };
    params.agent.radius = 0.0;
    let mut mesh = NavMeshBuilder::new(params)
        .build(&triangles)
        .expect("seed builds");
    assert_eq!(mesh.polys.len(), polys.len(), "one indexed slot per seed island");
    assert_eq!(
        mesh.polys_in_box(Vec3::splat(-1024.0), Vec3::splat(1024.0)),
        (0..polys.len() as u32).collect::<Vec<_>>()
    );
    mesh.polys = polys
        .iter()
        .map(|p| {
            assert!(p.len() >= 3);
            let mut area = 0.0;
            for (v, w) in edges(p) {
                assert!(v.x.abs() <= 128.0 && v.z.abs() <= 128.0);
                assert!(v.x.fract() == 0.0 && v.z.fract() == 0.0);
                area += cross(v.x, v.z, w.x, w.z);
                for u in p {
                    assert!(
                        cross(w.x - v.x, w.z - v.z, u.x - v.x, u.z - v.z) >= 0.0,
                        "convex CCW fixture"
                    );
                }
            }
            assert!(area > 0.0);
            let vertices: Vec<_> = p.iter().map(|v| v.vec3()).collect();
            let min = vertices
                .iter()
                .copied()
                .fold(Vec3::splat(f32::INFINITY), Vec3::min);
            let max = vertices
                .iter()
                .copied()
                .fold(Vec3::splat(f32::NEG_INFINITY), Vec3::max);
            NavPoly {
                vertices,
                min,
                max,
                area: 0,
                portals: Vec::new(),
            }
        })
        .collect();
    for (i, p) in polys.iter().enumerate() {
        for (j, q) in polys.iter().enumerate().skip(i + 1) {
            for (a, b) in edges(p) {
                if edges(q).any(|(c, d)| a == d && b == c) {
                    mesh.polys[i].portals.push(Portal {
                        neighbor: j as u32,
                        a: a.vec3(),
                        b: b.vec3(),
                    });
                    mesh.polys[j].portals.push(Portal {
                        neighbor: i as u32,
                        a: b.vec3(),
                        b: a.vec3(),
                    });
                }
            }
        }
    }
    // Author the subdivisions in chain order. This excludes multiple A* choices by construction.
    for (i, p) in mesh.polys.iter().enumerate() {
        let mut adjacent: Vec<_> = p.portals.iter().map(|p| p.neighbor as usize).collect();
        adjacent.sort_unstable();
        let expected: Vec<_> = (0..polys.len()).filter(|&j| i.abs_diff(j) == 1).collect();
        assert_eq!(adjacent, expected, "fixture must be a single polygon chain");
    }
    assert!(mesh.off_mesh_links.is_empty());
    assert_eq!(mesh.area_costs().changed().count(), 0);
    mesh
}

fn length_tolerance(segments: usize, reference: f64) -> f64 {
    // Inputs/corners are exactly representable. Allow f32 subtraction/sqrt and a rounded running
    // sum for the reported length: eight ulps per segment, scaled by total length (>= 1 m).
    // This is a measurement budget, NOT a tolerance for the funnel's orientation predicates.
    8.0 * f64::from(f32::EPSILON) * (segments + 1) as f64 * reference.max(1.0)
}

fn assert_path(polys: &[Vec<Point>], start: Point, end: Point, path: &NavPath) -> f64 {
    assert!(path.waypoints.len() >= 2);
    assert_eq!(path.waypoints.first().copied(), Some(start.vec3()));
    assert_eq!(path.waypoints.last().copied(), Some(end.vec3()));
    assert!(path.waypoints.iter().all(|p| p.is_finite() && p.y == 0.0));
    let points: Vec<_> = path.waypoints.iter().copied().map(Point::from_vec3).collect();
    for w in points.windows(2) {
        assert!(
            visible(polys, w[0], w[1]),
            "segment leaves corridor: {w:?}; {path:?}"
        );
    }
    let oracle = shortest_path(polys, start, end);
    assert!(oracle.windows(2).all(|w| visible(polys, w[0], w[1])));
    let optimum = polyline_length(&oracle);
    let measured = polyline_length(&points);
    let tolerance = length_tolerance(points.len() - 1, optimum);
    assert!(
        (measured - optimum).abs() <= tolerance,
        "waypoint length {measured} != visibility optimum {optimum} (tolerance {tolerance}); oracle {oracle:?}; {path:?}"
    );
    assert!(
        (f64::from(path.length) - measured).abs() <= tolerance,
        "reported length {} != measured {measured}; {path:?}",
        path.length
    );
    optimum
}

fn check_direction(polys: &[Vec<Point>], from: Point, to: Point) -> f64 {
    let mesh = mesh_from_polygons(polys);
    let query = NavQuery {
        mesh: &mesh,
        obstacles: None,
    };
    let (first, snapped_first) = query.find_nearest(from.vec3(), REACH).expect("on mesh");
    let (last, snapped_last) = query.find_nearest(to.vec3(), REACH).expect("on mesh");
    assert_eq!(snapped_first, from.vec3());
    assert_eq!(snapped_last, to.vec3());
    let expected: Vec<_> = if first <= last {
        (first..=last).collect()
    } else {
        (last..=first).rev().collect()
    };
    let path = query
        .find_path(from.vec3(), to.vec3(), REACH)
        .expect("unique route");
    assert_eq!(path.polys, expected);
    // Restrict BOTH containment and the reference graph to the actual selected corridor.
    let corridor: Vec<_> = path.polys.iter().map(|&i| polys[i as usize].clone()).collect();
    assert_path(&corridor, from, to, &path)
}

fn check_both_directions(polys: &[Vec<Point>], start: Point, end: Point) -> f64 {
    let forward = check_direction(polys, start, end);
    let reverse = check_direction(polys, end, start);
    assert!((forward - reverse).abs() < 1e-12);
    forward
}

fn straight() -> Vec<Vec<Point>> {
    (0..4).map(|i| rectangle(4 * i, 0, 4 * i + 4, 4)).collect()
}

fn elbow() -> Vec<Vec<Point>> {
    vec![
        rectangle(0, 0, 4, 2),
        rectangle(4, 0, 6, 2),
        rectangle(4, 2, 6, 8),
    ]
}

fn alternating() -> Vec<Vec<Point>> {
    let sections = [
        (0, 0),
        (4, 0),
        (8, 4),
        (12, 4),
        (16, 0),
        (20, 0),
        (24, 4),
        (28, 4),
    ];
    sections
        .windows(2)
        .map(|s| {
            let ((x0, z0), (x1, z1)) = (s[0], s[1]);
            polygon(&[(x0, z0), (x1, z1), (x1, z1 + 2), (x0, z0 + 2)])
        })
        .collect()
}

fn triangulate(quads: &[Vec<Point>], variant: usize) -> Vec<Vec<Point>> {
    quads
        .iter()
        .enumerate()
        .flat_map(|(i, p)| {
            assert_eq!(p.len(), 4);
            // Order triangles from the left vertical portal to the right one: both diagonals retain
            // a chain dual. Variant 2 alternates the diagonals within the same corridor.
            if variant == 0 || (variant == 2 && i.is_multiple_of(2)) {
                [vec![p[0], p[2], p[3]], vec![p[0], p[1], p[2]]]
            } else {
                [vec![p[0], p[1], p[3]], vec![p[1], p[2], p[3]]]
            }
        })
        .collect()
}

#[test]
fn straight_strip_has_euclidean_length() {
    let start = point(1, 1);
    let end = point(15, 3);
    let length = check_both_directions(&straight(), start, end);
    assert!((length - start.distance(end)).abs() < 1e-12);
}

#[test]
fn l_corridor_matches_analytic_reflex_corner_path() {
    let length = check_both_directions(&elbow(), point(1, 1), point(5, 7));
    assert!((length - (10.0_f64.sqrt() + 26.0_f64.sqrt())).abs() < 1e-12);
}

#[test]
fn alternating_narrow_turns_match_visibility_graph() {
    let length = check_both_directions(&alternating(), point(1, 1), point(27, 5));
    assert!(
        length > point(1, 1).distance(point(27, 5)) + 1.0,
        "turns must constrain the path"
    );
}

#[test]
fn different_polygon_triangulations_preserve_continuous_optimum() {
    for (quads, start, end) in [
        (straight(), point(1, 1), point(15, 3)),
        (alternating(), point(1, 1), point(27, 5)),
    ] {
        let base = check_both_directions(&quads, start, end);
        for variant in 0..3 {
            let length = check_both_directions(&triangulate(&quads, variant), start, end);
            assert!((base - length).abs() < 1e-12);
        }
    }
}

fn portal_endpoints(variant: Option<usize>, start: Point, end: Point) {
    let quads = alternating();
    let polys = match variant {
        None => quads,
        Some(v) => triangulate(&quads, v),
    };
    check_direction(&polys, start, end);
}

// Each matrix entry and travel direction is its own test so a production failure cannot hide
// the remaining portal cases. All assertions stay enabled, including the reproduced detours.
macro_rules! portal_pair {
    ($forward:ident, $reverse:ident, $variant:expr, $start:expr, $end:expr) => {
        #[test]
        fn $forward() {
            portal_endpoints($variant, $start, $end);
        }
        #[test]
        fn $reverse() {
            portal_endpoints($variant, $end, $start);
        }
    };
}

portal_pair!(
    portal_interior_quads_forward,
    portal_interior_quads_reverse,
    None,
    point(4, 1),
    point(24, 5)
);
portal_pair!(
    portal_interior_diagonal_a_forward,
    portal_interior_diagonal_a_reverse,
    Some(0),
    point(4, 1),
    point(24, 5)
);
portal_pair!(
    portal_interior_diagonal_b_forward,
    portal_interior_diagonal_b_reverse,
    Some(1),
    point(4, 1),
    point(24, 5)
);
portal_pair!(
    portal_interior_mixed_forward,
    portal_interior_mixed_reverse,
    Some(2),
    point(4, 1),
    point(24, 5)
);
portal_pair!(
    portal_vertex_quads_forward,
    portal_vertex_quads_reverse,
    None,
    point(4, 2),
    point(24, 4)
);
portal_pair!(
    portal_vertex_diagonal_a_forward,
    portal_vertex_diagonal_a_reverse,
    Some(0),
    point(4, 2),
    point(24, 4)
);
portal_pair!(
    portal_vertex_diagonal_b_forward,
    portal_vertex_diagonal_b_reverse,
    Some(1),
    point(4, 2),
    point(24, 4)
);
portal_pair!(
    portal_vertex_mixed_forward,
    portal_vertex_mixed_reverse,
    Some(2),
    point(4, 2),
    point(24, 4)
);
portal_pair!(
    same_portal_quads_forward,
    same_portal_quads_reverse,
    None,
    point(4, 1),
    point(4, 2)
);
portal_pair!(
    same_portal_diagonal_a_forward,
    same_portal_diagonal_a_reverse,
    Some(0),
    point(4, 1),
    point(4, 2)
);
portal_pair!(
    same_portal_diagonal_b_forward,
    same_portal_diagonal_b_reverse,
    Some(1),
    point(4, 1),
    point(4, 2)
);
portal_pair!(
    same_portal_mixed_forward,
    same_portal_mixed_reverse,
    Some(2),
    point(4, 1),
    point(4, 2)
);

#[test]
fn collinear_boundary_vertices_do_not_lengthen_paths() {
    for (quads, start, end) in [
        (straight(), point(1, 1), point(15, 3)),
        (alternating(), point(1, 1), point(27, 5)),
    ] {
        let base = check_both_directions(&quads, start, end);
        let subdivided: Vec<_> = quads
            .iter()
            .map(|p| {
                // Split the two longitudinal boundary edges, leaving portal edges whole.
                vec![
                    p[0],
                    Point {
                        x: (p[0].x + p[1].x) / 2.0,
                        z: (p[0].z + p[1].z) / 2.0,
                    },
                    p[1],
                    p[2],
                    Point {
                        x: (p[2].x + p[3].x) / 2.0,
                        z: (p[2].z + p[3].z) / 2.0,
                    },
                    p[3],
                ]
            })
            .collect();
        let length = check_both_directions(&subdivided, start, end);
        assert!((base - length).abs() < 1e-12);
    }
}

#[test]
fn reference_segment_checks_include_boundaries_and_reject_exterior_chords() {
    let polys = elbow();
    assert!(
        visible(&polys, point(0, 0), point(6, 0)),
        "along collinear exterior edges"
    );
    assert!(
        visible(&polys, point(4, 0), point(4, 8)),
        "along portal then boundary"
    );
    assert!(visible(&polys, point(1, 1), point(4, 2)), "ends at reflex vertex");
    assert!(
        visible(&polys, point(4, 2), point(5, 7)),
        "starts at reflex vertex"
    );
    for (a, b) in [(point(2, 0), point(6, 4)), (point(6, 4), point(2, 0))] {
        assert!(visible(&polys, a, b), "touches reflex vertex mid-segment");
    }
    // A length-measurement tolerance must never dilate the corridor. The f32 value immediately
    // left of x=4 is exterior above the elbow, even though it is less than a micrometer away.
    let just_outside = Point {
        x: f64::from(4.0_f32.next_down()),
        z: 3.0,
    };
    assert!(!visible(&polys, just_outside, just_outside));
    assert!(
        visible(&polys, point(4, 2), point(4, 2)),
        "zero-length boundary segment"
    );
    assert!(
        !visible(&polys, point(1, 1), point(5, 7)),
        "endpoints inside, chord outside"
    );
    assert!(!visible(&polys, point(1, 1), point(3, 3)), "end outside");
    assert!(!visible(&polys, point(3, 3), point(1, 1)), "start outside");
    assert!(
        !visible(&polys, point(3, 3), point(3, 3)),
        "zero-length outside segment"
    );
}
