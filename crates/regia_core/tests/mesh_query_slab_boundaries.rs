// SPDX-License-Identifier: MPL-2.0
//! Independent brute-force regressions for BVH slab boundaries and ray distance limits.

use glam::Vec3;
use regia_core::math::Ray;
use regia_core::mesh_query::{QueryCounts, TriBvh, TriMesh};

const DISTANCE_TOLERANCE: f32 = 2.0e-6;
const BARYCENTRIC_TOLERANCE: f32 = 2.0e-6;

#[derive(Debug, Clone, Copy)]
struct OracleHit {
    triangle: u32,
    distance: f64,
    barycentric: [f64; 3],
}

fn brute_force(mesh: &TriMesh, ray: &Ray, max_distance: f32) -> Vec<OracleHit> {
    let o = ray.origin.as_dvec3();
    let d = ray.direction.as_dvec3();
    let mut hits = Vec::new();
    for triangle in 0..mesh.triangle_count() as u32 {
        let [a, b, c] = mesh.triangle(triangle).map(Vec3::as_dvec3);
        let ab = b - a;
        let ac = c - a;
        let normal = ab.cross(ac);
        let denominator = normal.dot(d);
        if normal.length_squared() == 0.0 || denominator == 0.0 {
            continue;
        }
        let distance = normal.dot(a - o) / denominator;
        if distance < 0.0 || distance > f64::from(max_distance) || !distance.is_finite() {
            continue;
        }
        let p = o + d * distance - a;
        let (aa, ab_dot, bb, pa, pb) = (ab.dot(ab), ab.dot(ac), ac.dot(ac), p.dot(ab), p.dot(ac));
        let divisor = aa * bb - ab_dot * ab_dot;
        if divisor == 0.0 {
            continue;
        }
        let v = (bb * pa - ab_dot * pb) / divisor;
        let w = (aa * pb - ab_dot * pa) / divisor;
        let u = 1.0 - v - w;
        if u >= -1.0e-9 && v >= -1.0e-9 && w >= -1.0e-9 {
            hits.push(OracleHit {
                triangle,
                distance,
                barycentric: [u, v, w],
            });
        }
    }
    hits
}

fn assert_query_matches_oracle(bvh: &TriBvh, ray: Ray, max_distance: f32) -> QueryCounts {
    let expected = brute_force(bvh.mesh(), &ray, max_distance);
    let nearest = expected.iter().map(|hit| hit.distance).min_by(f64::total_cmp);
    let mut counts = QueryCounts::default();
    let counted = bvh.ray_cast_counted(&ray, max_distance, &mut counts);
    let plain = bvh.ray_cast(&ray, max_distance);
    assert_eq!(counted, plain, "counted and plain query disagree");
    match (nearest, plain) {
        (None, None) => {}
        (Some(distance), Some(hit)) => {
            let tied: Vec<_> = expected
                .iter()
                .filter(|candidate| (candidate.distance - distance).abs() <= 1.0e-9)
                .collect();
            assert!(
                tied.iter().any(|candidate| candidate.triangle == hit.triangle),
                "triangle {} not among tied oracle hits {tied:?}",
                hit.triangle
            );
            assert!((hit.distance - distance as f32).abs() <= DISTANCE_TOLERANCE);
            assert!(hit.point.is_finite() && hit.barycentric.is_finite());
            let oracle = tied
                .iter()
                .find(|candidate| candidate.triangle == hit.triangle)
                .expect("the returned triangle is tied");
            for (actual, expected) in hit.barycentric.to_array().into_iter().zip(oracle.barycentric) {
                assert!((f64::from(actual) - expected).abs() <= f64::from(BARYCENTRIC_TOLERANCE));
            }
            assert!((hit.barycentric.element_sum() - 1.0).abs() <= BARYCENTRIC_TOLERANCE);
        }
        (expected, actual) => panic!("oracle expected {expected:?}, BVH returned {actual:?}"),
    }
    counts
}

fn closed_box(min: Vec3, max: Vec3) -> TriMesh {
    let points = [
        Vec3::new(min.x, min.y, min.z),
        Vec3::new(max.x, min.y, min.z),
        Vec3::new(max.x, max.y, min.z),
        Vec3::new(min.x, max.y, min.z),
        Vec3::new(min.x, min.y, max.z),
        Vec3::new(max.x, min.y, max.z),
        Vec3::new(max.x, max.y, max.z),
        Vec3::new(min.x, max.y, max.z),
    ];
    let triangles = [
        [0, 2, 1],
        [0, 3, 2],
        [4, 5, 6],
        [4, 6, 7],
        [0, 1, 5],
        [0, 5, 4],
        [1, 2, 6],
        [1, 6, 5],
        [2, 3, 7],
        [2, 7, 6],
        [3, 0, 4],
        [3, 4, 7],
    ];
    TriMesh::new(points.to_vec(), triangles.to_vec()).expect("valid closed box")
}

#[test]
fn slab_faces_edges_corners_and_direction_signs_match_brute_force() {
    let mesh = closed_box(Vec3::splat(-1.0), Vec3::ONE);
    let bvh = TriBvh::build(mesh).expect("box tree");
    let mut total = QueryCounts::default();

    for axis in 0..3 {
        let other: Vec<_> = (0..3).filter(|&candidate| candidate != axis).collect();
        for face_side in [-1.0, 1.0] {
            for mask in 0..4 {
                for sign in [-1.0, 1.0] {
                    let mut origin = Vec3::ZERO;
                    origin[axis] = face_side;
                    origin[other[0]] = if mask & 1 == 0 { 0.0 } else { 1.0 };
                    origin[other[1]] = if mask & 2 == 0 { 0.0 } else { -1.0 };
                    let mut direction = Vec3::ZERO;
                    direction[axis] = sign;
                    direction[other[0]] = if axis + other[0] == 2 { -0.0 } else { 0.0 };
                    direction[other[1]] = if axis + other[1] == 2 { -0.0 } else { 0.0 };
                    total = total.plus(assert_query_matches_oracle(
                        &bvh,
                        Ray::new(origin, direction),
                        4.0,
                    ));
                }
            }
        }
        for mask in 0..4 {
            for side in [-1.0, 1.0] {
                for sign in [-1.0, 1.0] {
                    let mut origin = Vec3::ZERO;
                    origin[axis] = side * 2.0;
                    origin[other[0]] = if mask & 1 == 0 { 0.0 } else { 1.0 };
                    origin[other[1]] = if mask & 2 == 0 { 0.0 } else { -1.0 };
                    let mut direction = Vec3::ZERO;
                    direction[axis] = -side * sign;
                    // Keep both signed zeros in the parallel slab components.
                    direction[other[0]] = if axis + other[0] == 2 { -0.0 } else { 0.0 };
                    direction[other[1]] = if axis + other[1] == 2 { -0.0 } else { 0.0 };
                    total = total.plus(assert_query_matches_oracle(
                        &bvh,
                        Ray::new(origin, direction),
                        4.0,
                    ));
                }
            }
        }
    }
    assert!(
        total.nodes > 0 && total.triangles > 0,
        "traversal counts: {total:?}"
    );
    eprintln!("closed-box slab regression traversal counts: {total:?}");
}

#[test]
fn separated_triangles_scale_translate_and_distance_limits_match_oracle() {
    let base = TriMesh::new(
        vec![
            Vec3::new(-0.5, 0.0, 0.0),
            Vec3::new(0.5, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(3.0, -1.0, 0.0),
            Vec3::new(5.0, -1.0, 0.0),
            Vec3::new(4.0, 1.0, 0.0),
            Vec3::new(-1.0, -1.0, 0.0),
            Vec3::new(1.0, -1.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ],
        vec![[0, 1, 2], [3, 4, 5], [6, 7, 8]],
    )
    .expect("separated triangles and one degenerate segment");
    let mut total = QueryCounts::default();
    for (scale, translation) in [
        (1.0, Vec3::ZERO),
        (0.5, Vec3::new(8.0, -4.0, 2.0)),
        (4.0, Vec3::new(-16.0, 8.0, -4.0)),
    ] {
        let positions = base
            .positions()
            .iter()
            .map(|point| *point * scale + translation)
            .collect();
        let mesh = TriMesh::new(positions, base.triangles().to_vec()).expect("transformed mesh");
        let bvh = TriBvh::build(mesh).expect("triangle tree");
        for origin in [
            Vec3::new(translation.x, translation.y, translation.z + 2.0 * scale),
            Vec3::new(
                translation.x + 4.0 * scale,
                translation.y,
                translation.z + 2.0 * scale,
            ),
        ] {
            let ray = Ray::new(origin, Vec3::new(-0.0, 0.0, -1.0));
            let exact_distance = 2.0 * scale;
            let just_below = f32::from_bits(exact_distance.to_bits() - 1);
            let just_above = f32::from_bits(exact_distance.to_bits() + 1);
            for max_distance in [just_below, exact_distance, just_above] {
                total = total.plus(assert_query_matches_oracle(&bvh, ray, max_distance));
            }
        }
    }
    assert!(
        total.nodes > 0 && total.triangles > 0,
        "traversal counts: {total:?}"
    );
    eprintln!("separated-triangle traversal counts: {total:?}");
}
