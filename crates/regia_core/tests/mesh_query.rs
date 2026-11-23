// SPDX-License-Identifier: MPL-2.0
//! Mesh spatial queries (`regia_core::mesh_query`, ADR-1461): every tree query is checked against
//! a brute-force loop over the triangles, winding numbers against the exact sum and against known
//! inside/outside answers, and the distance grid against the analytic distance to a sphere. All
//! inputs are generated from fixed seeds.

use regia_core::glam::{Affine3A, Quat, Vec2, Vec3};
use regia_core::math::{Aabb, Ray, Rng, Sphere};
use regia_core::mesh_query::{
    MeshQueryError, PointHash, SdfParams, SignedDistanceGrid, TriBvh, TriMesh, WindingNumberTree, triangle,
    winding_number_exact,
};

// ---- meshes ------------------------------------------------------------------------------

/// Turns every triangle so its normal points along `outward(centroid)`.
fn orient(positions: &[Vec3], tris: &mut [[u32; 3]], outward: impl Fn(Vec3) -> Vec3) {
    for t in tris.iter_mut() {
        let [a, b, c] = t.map(|i| positions[i as usize]);
        if (b - a).cross(c - a).dot(outward((a + b + c) / 3.0)) < 0.0 {
            t.swap(1, 2);
        }
    }
}

/// A unit cube `[0, 1]³`, 12 triangles, outward.
fn cube() -> TriMesh {
    let mut positions = Vec::new();
    for i in 0..8u32 {
        positions.push(Vec3::new(
            (i & 1) as f32,
            ((i >> 1) & 1) as f32,
            ((i >> 2) & 1) as f32,
        ));
    }
    let faces = [
        [0, 1, 3, 2],
        [4, 6, 7, 5],
        [0, 4, 5, 1],
        [2, 3, 7, 6],
        [0, 2, 6, 4],
        [1, 5, 7, 3],
    ];
    let mut tris = Vec::new();
    for [a, b, c, d] in faces {
        tris.push([a, b, c]);
        tris.push([a, c, d]);
    }
    orient(&positions, &mut tris, |c| c - Vec3::splat(0.5));
    TriMesh::new(positions, tris).expect("cube")
}

/// A latitude/longitude sphere of `radius` at the origin: `2 · slices · (stacks − 1)` triangles.
fn uv_sphere(radius: f32, slices: u32, stacks: u32) -> TriMesh {
    let mut positions = vec![Vec3::Y * radius, -Vec3::Y * radius];
    for s in 1..stacks {
        let phi = std::f32::consts::PI * s as f32 / stacks as f32;
        for l in 0..slices {
            let theta = std::f32::consts::TAU * l as f32 / slices as f32;
            positions.push(Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()) * radius);
        }
    }
    let ring = |s: u32, l: u32| 2 + (s - 1) * slices + (l % slices);
    let mut tris = Vec::new();
    for l in 0..slices {
        tris.push([0, ring(1, l), ring(1, l + 1)]);
        tris.push([1, ring(stacks - 1, l + 1), ring(stacks - 1, l)]);
    }
    for s in 1..stacks - 1 {
        for l in 0..slices {
            let (a, b, c, d) = (ring(s, l), ring(s, l + 1), ring(s + 1, l), ring(s + 1, l + 1));
            tris.push([a, c, b]);
            tris.push([b, c, d]);
        }
    }
    orient(&positions, &mut tris, |c| c);
    TriMesh::new(positions, tris).expect("sphere")
}

/// A torus around the y axis: core radius `big`, tube radius `small`.
fn torus(big: f32, small: f32, around: u32, tube: u32) -> TriMesh {
    let mut positions = Vec::new();
    for i in 0..around {
        let u = std::f32::consts::TAU * i as f32 / around as f32;
        for j in 0..tube {
            let v = std::f32::consts::TAU * j as f32 / tube as f32;
            let r = big + small * v.cos();
            positions.push(Vec3::new(r * u.cos(), small * v.sin(), r * u.sin()));
        }
    }
    let at = |i: u32, j: u32| (i % around) * tube + (j % tube);
    let mut tris = Vec::new();
    for i in 0..around {
        for j in 0..tube {
            tris.push([at(i, j), at(i + 1, j), at(i, j + 1)]);
            tris.push([at(i + 1, j), at(i + 1, j + 1), at(i, j + 1)]);
        }
    }
    orient(&positions, &mut tris, |c| {
        let core = Vec3::new(c.x, 0.0, c.z).normalize_or_zero() * big;
        c - core
    });
    TriMesh::new(positions, tris).expect("torus")
}

/// A random soup of `n` triangles in `[-1, 1]³`, every tenth one degenerate.
fn soup(seed: u64, n: u32) -> TriMesh {
    let mut rng = Rng::new(seed);
    let mut positions = Vec::new();
    let mut tris = Vec::new();
    for t in 0..n {
        let center = Vec3::new(rng.range(-1.0..1.0), rng.range(-1.0..1.0), rng.range(-1.0..1.0));
        let size: f32 = rng.range(0.02..0.3);
        let a = center + rng.in_sphere() * size;
        let b = center + rng.in_sphere() * size;
        let c = match t % 10 {
            0 => a.lerp(b, 0.3), // collinear
            5 => a,              // two equal vertices
            _ => center + rng.in_sphere() * size,
        };
        let base = positions.len() as u32;
        positions.extend([a, b, c]);
        tris.push([base, base + 1, base + 2]);
    }
    TriMesh::new(positions, tris).expect("soup")
}

fn random_point(rng: &mut Rng, half: f32) -> Vec3 {
    Vec3::new(
        rng.range(-half..half),
        rng.range(-half..half),
        rng.range(-half..half),
    )
}

// ---- brute force -------------------------------------------------------------------------

fn brute_closest(mesh: &TriMesh, p: Vec3) -> f32 {
    (0..mesh.triangle_count() as u32)
        .map(|t| {
            let [a, b, c] = mesh.triangle(t);
            triangle::closest_point(p, a, b, c).0.distance(p)
        })
        .fold(f32::INFINITY, f32::min)
}

fn brute_ray(mesh: &TriMesh, ray: &Ray) -> Option<f32> {
    (0..mesh.triangle_count() as u32)
        .filter_map(|t| {
            let [a, b, c] = mesh.triangle(t);
            triangle::ray_hit(ray.origin, ray.direction, a, b, c).map(|h| h.0)
        })
        .reduce(f32::min)
}

// ---- errors ------------------------------------------------------------------------------

#[test]
fn empty_mesh_errors_are_typed() {
    assert_eq!(
        TriBvh::build(TriMesh::default()).expect_err("bvh"),
        MeshQueryError::EmptyMesh
    );
    assert_eq!(
        WindingNumberTree::build(TriMesh::default()).expect_err("winding"),
        MeshQueryError::EmptyMesh
    );
    assert_eq!(
        SignedDistanceGrid::from_mesh(TriMesh::default(), &SdfParams::new(0.1)).expect_err("sdf"),
        MeshQueryError::EmptyMesh
    );
    assert_eq!(winding_number_exact(&TriMesh::default(), Vec3::ZERO), 0.0);
}

#[test]
fn bad_meshes_and_parameters_are_rejected() {
    let p = vec![Vec3::ZERO, Vec3::X, Vec3::Y];
    assert_eq!(
        TriMesh::new(p.clone(), vec![[0, 1, 3]]).expect_err("range"),
        MeshQueryError::IndexOutOfRange {
            triangle: 0,
            index: 3,
            vertex_count: 3
        }
    );
    assert_eq!(
        TriMesh::new(vec![Vec3::ZERO, Vec3::NAN, Vec3::Y], vec![[0, 1, 2]]).expect_err("nan"),
        MeshQueryError::NonFinite(1)
    );
    assert_eq!(
        TriMesh::from_indices(p.clone(), &[0, 1]).expect_err("count"),
        MeshQueryError::IndexCount(2)
    );
    let mesh = TriMesh::from_indices(p, &[0, 1, 2]).expect("flat indices");
    assert_eq!(mesh.triangle_count(), 1);
    let bvh = TriBvh::build(mesh.clone()).expect("bvh");
    assert_eq!(
        WindingNumberTree::from_bvh(bvh.clone(), 0.0).expect_err("beta"),
        MeshQueryError::InvalidBeta(0.0)
    );
    assert!(WindingNumberTree::from_bvh(bvh, f32::NAN).is_err());
    for h in [0.0, -1.0, f32::INFINITY] {
        assert_eq!(
            SignedDistanceGrid::from_mesh(mesh.clone(), &SdfParams::new(h)).expect_err("cell"),
            MeshQueryError::InvalidCellSize(h)
        );
    }
    let mut tiny = SdfParams::new(0.01);
    tiny.max_points = 1000;
    assert!(matches!(
        SignedDistanceGrid::from_mesh(mesh, &tiny),
        Err(MeshQueryError::GridTooLarge { limit: 1000, .. })
    ));
    assert_eq!(
        PointHash::build(&[], 0.0).expect_err("cell"),
        MeshQueryError::InvalidCellSize(0.0)
    );
    assert_eq!(
        PointHash::build(&[Vec3::ZERO, Vec3::INFINITY], 1.0).expect_err("inf"),
        MeshQueryError::NonFinite(1)
    );
}

// ---- TriBvh ------------------------------------------------------------------------------

#[test]
fn closest_point_matches_brute_force_on_random_soups() {
    for seed in [1, 2, 3] {
        let mesh = soup(seed, 600);
        let bvh = TriBvh::build(mesh.clone()).expect("bvh");
        let mut rng = Rng::new(seed + 100);
        for _ in 0..300 {
            let p = random_point(&mut rng, 1.6);
            let hit = bvh.closest_point(p, f32::INFINITY).expect("closest");
            let want = brute_closest(&mesh, p);
            assert!(
                (hit.distance - want).abs() <= 1e-5 * (1.0 + want),
                "seed {seed} {p}: {} vs {want}",
                hit.distance
            );
            // The reported point is on the reported triangle and at the reported distance.
            let [a, b, c] = mesh.triangle(hit.triangle);
            let q = a * hit.barycentric.x + b * hit.barycentric.y + c * hit.barycentric.z;
            assert!(q.distance(hit.point) < 1e-4, "{q} vs {}", hit.point);
            assert!((hit.point.distance(p) - hit.distance).abs() < 1e-4);
        }
    }
}

#[test]
fn closest_point_respects_the_distance_limit() {
    let bvh = TriBvh::build(cube()).expect("bvh");
    let p = Vec3::new(0.5, 0.5, 3.0);
    assert!(bvh.closest_point(p, 1.0).is_none());
    let hit = bvh.closest_point(p, 2.0).expect("within 2");
    assert!((hit.distance - 2.0).abs() < 1e-6);
    assert!((hit.point - Vec3::new(0.5, 0.5, 1.0)).length() < 1e-6);
    assert!(bvh.closest_point(Vec3::NAN, f32::INFINITY).is_none());
}

#[test]
fn ray_cast_matches_brute_force_on_random_soups() {
    for seed in [4, 5] {
        let mesh = soup(seed, 800);
        let bvh = TriBvh::build(mesh.clone()).expect("bvh");
        let mut rng = Rng::new(seed + 200);
        let mut hits = 0;
        for n in 0..400 {
            let origin = random_point(&mut rng, 2.0);
            // Half the rays aim at a triangle, half go anywhere.
            let direction = if n % 2 == 0 {
                let [a, b, c] = mesh.triangle(rng.range(0..mesh.triangle_count() as u32));
                (a + b + c) / 3.0 - origin
            } else {
                rng.unit_vector()
            };
            let ray = Ray::new(origin, direction);
            let got = bvh.ray_cast(&ray, f32::INFINITY);
            let want = brute_ray(&mesh, &ray);
            match (got, want) {
                (Some(h), Some(w)) => {
                    hits += 1;
                    assert!(
                        (h.distance - w).abs() <= 1e-5 * (1.0 + w),
                        "{} vs {w}",
                        h.distance
                    );
                    assert!(h.point.distance(ray.at(h.distance)) < 1e-4);
                }
                (None, None) => {}
                other => panic!("seed {seed} ray {ray:?}: {other:?}"),
            }
            // A limit just short of the hit misses; at the hit it hits.
            if let Some(w) = want {
                assert!(bvh.ray_cast(&ray, w * 0.999 - 1e-4).is_none());
                assert!(bvh.ray_cast(&ray, w * 1.001 + 1e-4).is_some());
            }
        }
        assert!(hits > 150, "too few hits ({hits}) for the test to mean much");
    }
}

#[test]
fn rays_hit_the_cube_faces_and_edges() {
    let bvh = TriBvh::build(cube()).expect("bvh");
    let hit = bvh
        .ray_cast(&Ray::new(Vec3::new(0.3, 0.6, -2.0), Vec3::Z), f32::INFINITY)
        .expect("face");
    assert!((hit.distance - 2.0).abs() < 1e-6);
    // Through the diagonal shared by a face's two triangles, and along an axis-parallel direction.
    let hit = bvh
        .ray_cast(&Ray::new(Vec3::new(0.5, 0.5, 5.0), -Vec3::Z), f32::INFINITY)
        .expect("diagonal");
    assert!((hit.distance - 4.0).abs() < 1e-6);
    // From inside, the nearest face is the far side of the ray.
    let hit = bvh
        .ray_cast(&Ray::new(Vec3::splat(0.5), Vec3::X), f32::INFINITY)
        .expect("inside");
    assert!((hit.distance - 0.5).abs() < 1e-6);
    assert!(
        bvh.ray_cast(&Ray::new(Vec3::splat(2.0), Vec3::X), f32::INFINITY)
            .is_none()
    );
    assert!(
        bvh.ray_cast(&Ray::new(Vec3::ZERO, Vec3::ZERO), f32::INFINITY)
            .is_none()
    );
}

#[test]
fn box_and_sphere_queries_match_brute_force() {
    let mesh = soup(7, 700);
    let bvh = TriBvh::build(mesh.clone()).expect("bvh");
    let mut rng = Rng::new(77);
    for _ in 0..100 {
        let c = random_point(&mut rng, 1.2);
        let h = Vec3::new(rng.range(0.0..0.5), rng.range(0.0..0.5), rng.range(0.0..0.5));
        let aabb = Aabb::from_center_half_extents(c, h);
        let want: Vec<u32> = (0..mesh.triangle_count() as u32)
            .filter(|&t| {
                let [a, b, cc] = mesh.triangle(t);
                triangle::overlaps_aabb(a, b, cc, aabb.min, aabb.max)
            })
            .collect();
        assert_eq!(bvh.triangles_in_aabb(&aabb), want);
        let sphere = Sphere::new(c, rng.range(0.0..0.6));
        let want: Vec<u32> = (0..mesh.triangle_count() as u32)
            .filter(|&t| {
                let [a, b, cc] = mesh.triangle(t);
                triangle::closest_point(c, a, b, cc).0.distance(c) <= sphere.radius
            })
            .collect();
        assert_eq!(bvh.triangles_in_sphere(&sphere), want);
    }
    assert!(bvh.triangles_in_aabb(&Aabb::EMPTY).is_empty());
    assert!(bvh.triangles_in_sphere(&Sphere::new(Vec3::ZERO, -1.0)).is_empty());
    // The whole mesh is in its own box.
    assert_eq!(bvh.triangles_in_aabb(&bvh.bounds()).len(), mesh.triangle_count());
}

#[test]
fn tree_intersection_matches_brute_force_pairs() {
    let a = TriBvh::build(uv_sphere(1.0, 16, 10)).expect("a");
    let b_mesh = uv_sphere(0.7, 12, 8);
    let b = TriBvh::build(b_mesh.clone()).expect("b");
    for (offset, rotation) in [
        (Vec3::new(1.5, 0.0, 0.0), Quat::IDENTITY),
        (Vec3::new(1.2, 0.4, -0.3), Quat::from_rotation_y(0.7)),
        (Vec3::new(3.0, 0.0, 0.0), Quat::from_rotation_x(1.1)),
        (Vec3::ZERO, Quat::IDENTITY), // b entirely inside a: no surface contact
    ] {
        let xf = Affine3A::from_rotation_translation(rotation, offset);
        let mut want = Vec::new();
        for ta in 0..a.mesh().triangle_count() as u32 {
            for tb in 0..b_mesh.triangle_count() as u32 {
                let tri_b = b_mesh.triangle(tb).map(|p| xf.transform_point3(p));
                if triangle::triangles_intersect(a.mesh().triangle(ta), tri_b) {
                    want.push((ta, tb));
                }
            }
        }
        let got = a.intersecting_pairs(&b, Some(&xf));
        assert_eq!(got, want, "offset {offset}");
        assert_eq!(a.intersects(&b, Some(&xf)), !want.is_empty());
    }
    // Without a transform both trees are in one space: concentric spheres do not touch.
    assert!(!a.intersects(&b, None));
    assert!(a.intersects(&a, None), "a mesh touches itself");
}

#[test]
fn degenerate_triangles_are_handled_everywhere() {
    // A cube plus slivers and points inside and on it.
    let base = cube();
    let mut positions = base.positions().to_vec();
    let mut tris = base.triangles().to_vec();
    let n = positions.len() as u32;
    positions.extend([
        Vec3::new(0.2, 0.2, 0.2),
        Vec3::new(0.8, 0.8, 0.8),
        Vec3::new(0.5, 0.5, 0.5),
        Vec3::new(0.5, 0.5, 1.0),
    ]);
    tris.push([n, n + 1, n + 2]); // collinear sliver through the middle
    tris.push([n + 3, n + 3, n + 3]); // a point on the top face
    tris.push([n, n, n + 1]); // two equal corners
    let mesh = TriMesh::new(positions, tris).expect("mesh");
    let bvh = TriBvh::build(mesh.clone()).expect("bvh");
    let hit = bvh
        .closest_point(Vec3::new(0.45, 0.5, 0.5), f32::INFINITY)
        .expect("closest");
    assert!(hit.distance < 0.06, "the sliver is closest: {hit:?}");
    assert!((hit.distance - brute_closest(&mesh, Vec3::new(0.45, 0.5, 0.5))).abs() < 1e-6);
    let hit = bvh
        .ray_cast(&Ray::new(Vec3::new(0.5, 0.5, 3.0), -Vec3::Z), f32::INFINITY)
        .expect("ray");
    assert!(
        (hit.distance - 2.0).abs() < 1e-6,
        "slivers are never hit: {hit:?}"
    );
    let tree = WindingNumberTree::from_bvh(bvh, 2.0).expect("winding");
    let w = tree.winding_number(Vec3::splat(0.5));
    assert!((w - 1.0).abs() < 1e-3, "zero-area triangles add nothing: {w}");
    assert!(tree.winding_number(Vec3::new(0.5, 0.5, 1.0)).is_finite());
    let sdf = SignedDistanceGrid::build(&tree, &SdfParams::new(0.1)).expect("sdf");
    assert!(sdf.grid().values().iter().all(|v| v.is_finite()));
}

// ---- winding numbers ---------------------------------------------------------------------

#[test]
fn exact_winding_number_is_one_inside_and_zero_outside() {
    let sphere = uv_sphere(1.0, 24, 12);
    let t = torus(2.0, 0.5, 32, 16);
    for p in [Vec3::ZERO, Vec3::new(0.5, 0.1, -0.3), Vec3::new(0.0, 0.9, 0.0)] {
        assert!((winding_number_exact(&sphere, p) - 1.0).abs() < 1e-4, "{p}");
    }
    for p in [
        Vec3::new(1.5, 0.0, 0.0),
        Vec3::new(0.0, -3.0, 0.5),
        Vec3::splat(10.0),
    ] {
        assert!(winding_number_exact(&sphere, p).abs() < 1e-4, "{p}");
    }
    // The torus: the hole is outside, the tube is inside.
    assert!(winding_number_exact(&t, Vec3::ZERO).abs() < 1e-4);
    assert!((winding_number_exact(&t, Vec3::new(2.0, 0.0, 0.0)) - 1.0).abs() < 1e-4);
    assert!((winding_number_exact(&t, Vec3::new(0.0, 0.2, -2.1)) - 1.0).abs() < 1e-4);
    // Flipping the mesh negates the winding number.
    let mut flipped = sphere.clone();
    flipped.flip();
    assert!((winding_number_exact(&flipped, Vec3::ZERO) + 1.0).abs() < 1e-4);
}

/// Largest |fast − exact| over random points in and around `mesh`'s box.
fn max_fast_error(mesh: &TriMesh, beta: f32, seed: u64, samples: usize) -> f32 {
    let bvh = TriBvh::build(mesh.clone()).expect("bvh");
    let tree = WindingNumberTree::from_bvh(bvh, beta).expect("tree");
    let b = mesh.bounds();
    let mut rng = Rng::new(seed);
    let mut worst = 0.0_f32;
    for _ in 0..samples {
        let u = Vec3::new(rng.range(-0.5..1.5), rng.range(-0.5..1.5), rng.range(-0.5..1.5));
        let p = b.min + u * b.size();
        let err = (tree.winding_number(p) - winding_number_exact(mesh, p)).abs();
        worst = worst.max(err);
    }
    worst
}

#[test]
fn fast_winding_number_is_within_its_error_bound() {
    let sphere = uv_sphere(1.0, 48, 24);
    let t = torus(1.5, 0.5, 48, 24);
    let s = soup(9, 1500);
    for (name, mesh) in [("sphere", &sphere), ("torus", &t), ("soup", &s)] {
        // Measured on these inputs (lane report): sphere 0.041 / 0.0089 / 0.00046, torus 0.020 /
        // 0.0018 / 0.00028, soup 0.0024 / 0.00018 / 0.000015 for beta 2 / 4 / 8. The truncation
        // error of the second-order expansion falls about as (1 / beta)³.
        let e2 = max_fast_error(mesh, 2.0, 11, 400);
        let e4 = max_fast_error(mesh, 4.0, 11, 400);
        let e8 = max_fast_error(mesh, 8.0, 11, 400);
        assert!(e2 < 0.06, "{name}: beta 2 max error {e2}");
        assert!(e4 < 0.015, "{name}: beta 4 max error {e4}");
        assert!(e8 < 0.002, "{name}: beta 8 max error {e8}");
        assert!(
            e8 < e4 && e4 < e2,
            "{name}: a larger beta must be more accurate ({e2}, {e4}, {e8})"
        );
    }
}

#[test]
fn inside_outside_on_watertight_holed_and_flipped_meshes() {
    let mesh = uv_sphere(1.0, 32, 16);
    let watertight = WindingNumberTree::build(mesh.clone()).expect("tree");
    let inside = [Vec3::ZERO, Vec3::new(0.6, 0.0, 0.0), Vec3::new(-0.2, -0.7, 0.3)];
    let outside = [
        Vec3::new(1.3, 0.0, 0.0),
        Vec3::new(0.0, -1.5, 0.0),
        Vec3::splat(4.0),
    ];
    for p in inside {
        assert!(watertight.is_inside(p), "{p}");
    }
    for p in outside {
        assert!(!watertight.is_inside(p), "{p}");
    }
    // A hole: drop every triangle around the north pole (y > 0.85).
    let tris: Vec<[u32; 3]> = mesh
        .triangles()
        .iter()
        .copied()
        .filter(|t| t.iter().all(|&i| mesh.positions()[i as usize].y < 0.85))
        .collect();
    assert!(tris.len() < mesh.triangle_count());
    let holed = WindingNumberTree::build(TriMesh::new(mesh.positions().to_vec(), tris).expect("holed"))
        .expect("tree");
    let w = holed.winding_number(Vec3::ZERO);
    assert!(w > 0.5 && w < 1.0, "a hole lowers but keeps the inside: {w}");
    for p in inside {
        assert!(holed.is_inside(p), "{p}");
    }
    for p in outside {
        assert!(!holed.is_inside(p), "{p}");
    }
    // Flipped faces: reverse a band of triangles around the equator on the -x side.
    let mut flipped_tris = mesh.triangles().to_vec();
    let mut flipped = 0;
    for t in &mut flipped_tris {
        let c = t.iter().map(|&i| mesh.positions()[i as usize]).sum::<Vec3>() / 3.0;
        if c.x < -0.9 {
            t.swap(1, 2);
            flipped += 1;
        }
    }
    assert!(flipped > 0);
    let tree =
        WindingNumberTree::build(TriMesh::new(mesh.positions().to_vec(), flipped_tris).expect("flipped"))
            .expect("tree");
    for p in inside {
        assert!(tree.is_inside(p), "{p}");
    }
    for p in outside {
        assert!(!tree.is_inside(p), "{p}");
    }
}

// ---- signed distance grid -----------------------------------------------------------------

#[test]
fn sphere_distance_grid_matches_the_analytic_distance() {
    let radius = 1.0;
    let mesh = uv_sphere(radius, 64, 32);
    let h = 0.1;
    let sdf = SignedDistanceGrid::from_mesh(mesh, &SdfParams::new(h)).expect("sdf");
    let dims = sdf.dims();
    assert!(dims.x >= 24 && dims.x <= 30, "{dims}");
    let mut checked = 0;
    for k in 0..dims.z {
        for j in 0..dims.y {
            for i in 0..dims.x {
                let p = sdf.point(i, j, k);
                let want = p.length() - radius;
                let got = sdf.value(i, j, k).expect("value");
                assert!((got - want).abs() <= h, "({i}, {j}, {k}) at {p}: {got} vs {want}");
                if want.abs() > h {
                    assert_eq!(got < 0.0, want < 0.0, "sign at {p}");
                }
                checked += 1;
            }
        }
    }
    assert_eq!(checked, dims.x * dims.y * dims.z);
    // Trilinear samples between grid points, and the gradient near the surface.
    let mut rng = Rng::new(31);
    for _ in 0..500 {
        let p = random_point(&mut rng, 1.15);
        let want = p.length() - radius;
        let got = sdf.sample(p).expect("inside the grid");
        assert!((got - want).abs() <= h, "{p}: {got} vs {want}");
        if want.abs() < 0.3 && p.length() > 0.5 {
            let g = sdf.gradient(p).expect("gradient");
            assert!(g.normalize_or_zero().dot(p.normalize()) > 0.9, "{p}: {g}");
            assert!((g.length() - 1.0).abs() < 0.3, "{p}: |{g}|");
        }
    }
    assert!(sdf.sample(Vec3::splat(10.0)).is_none());
    assert!(
        sdf.sample(sdf.bounds().max).is_some(),
        "the far corner is inside the grid"
    );
}

#[test]
fn cube_distance_grid_without_sweeping_keeps_the_band_exact() {
    let mut params = SdfParams::new(0.125);
    params.sweep = false;
    params.exact_band = 2;
    let sdf = SignedDistanceGrid::from_mesh(cube(), &params).expect("sdf");
    let center = sdf.sample(Vec3::splat(0.5)).expect("center");
    assert!(center < 0.0, "the center is inside: {center}");
    // On a face, half a cell out and half a cell in.
    let out = sdf.sample(Vec3::new(0.5, 0.5, 1.0625)).expect("out");
    let inn = sdf.sample(Vec3::new(0.5, 0.5, 0.9375)).expect("in");
    assert!((out - 0.0625).abs() < 1e-5, "{out}");
    assert!((inn + 0.0625).abs() < 1e-5, "{inn}");
    // With sweeping, the center gets its true distance.
    let swept = SignedDistanceGrid::from_mesh(cube(), &SdfParams::new(0.125)).expect("swept");
    let c = swept.sample(Vec3::splat(0.5)).expect("center");
    assert!((c + 0.5).abs() < 1e-5, "{c}");
}

#[test]
fn distance_grid_of_an_open_mesh_signs_by_winding_number() {
    // The sphere with its north cap removed: points deep inside stay negative.
    let mesh = uv_sphere(1.0, 32, 16);
    let tris: Vec<[u32; 3]> = mesh
        .triangles()
        .iter()
        .copied()
        .filter(|t| t.iter().all(|&i| mesh.positions()[i as usize].y < 0.9))
        .collect();
    let holed = TriMesh::new(mesh.positions().to_vec(), tris).expect("holed");
    let sdf = SignedDistanceGrid::from_mesh(holed, &SdfParams::new(0.1)).expect("sdf");
    assert!(sdf.sample(Vec3::ZERO).expect("center") < -0.8);
    assert!(sdf.sample(Vec3::new(0.0, -0.5, 0.0)).expect("lower") < 0.0);
    assert!(sdf.sample(Vec3::new(1.2, 0.0, 0.0)).expect("outside") > 0.0);
}

#[test]
fn torus_distance_grid_signs_the_tube_and_the_hole() {
    let (big, small) = (1.5, 0.5);
    let mesh = torus(big, small, 64, 32);
    let h = 0.1;
    let sdf = SignedDistanceGrid::from_mesh(mesh, &SdfParams::new(h)).expect("sdf");
    let analytic = |p: Vec3| {
        let ring = Vec3::new(p.x, 0.0, p.z).length() - big;
        Vec2::new(ring, p.y).length() - small
    };
    let mut rng = Rng::new(13);
    for _ in 0..800 {
        let p = Vec3::new(rng.range(-2.0..2.0), rng.range(-0.6..0.6), rng.range(-2.0..2.0));
        let want = analytic(p);
        let got = sdf.sample(p).expect("inside the grid");
        assert!((got - want).abs() <= h, "{p}: {got} vs {want}");
    }
    assert!(sdf.sample(Vec3::ZERO).expect("hole") > 0.9, "the hole is outside");
    assert!(
        sdf.sample(Vec3::new(big, 0.0, 0.0)).expect("tube") < -0.4,
        "the tube is inside"
    );
}

#[test]
fn queries_stay_accurate_far_from_the_origin() {
    // The same soup moved 10 km away: f64 internals keep the answers within f32 position precision.
    let offset = Vec3::new(10_000.0, -2_000.0, 7_500.0);
    let near = soup(21, 300);
    let far = TriMesh::new(
        near.positions().iter().map(|&p| p + offset).collect(),
        near.triangles().to_vec(),
    )
    .expect("far");
    let (bvh_near, bvh_far) = (
        TriBvh::build(near.clone()).expect("near"),
        TriBvh::build(far.clone()).expect("far"),
    );
    let mut rng = Rng::new(22);
    for _ in 0..200 {
        let p = random_point(&mut rng, 1.5);
        let a = bvh_near.closest_point(p, f32::INFINITY).expect("near");
        let b = bvh_far.closest_point(p + offset, f32::INFINITY).expect("far");
        assert!(
            (a.distance - b.distance).abs() < 5e-3,
            "{} vs {}",
            a.distance,
            b.distance
        );
        // The f32 brute force rounds at these coordinates (about 1e-3), the tree does not.
        assert!((b.distance - brute_closest(&far, p + offset)).abs() < 5e-3);
    }
    let sphere = uv_sphere(1.0, 24, 12);
    let moved = TriMesh::new(
        sphere.positions().iter().map(|&p| p + offset).collect(),
        sphere.triangles().to_vec(),
    )
    .expect("moved");
    let tree = WindingNumberTree::build(moved.clone()).expect("tree");
    assert!(tree.is_inside(offset));
    assert!(!tree.is_inside(offset + Vec3::X * 1.5));
    let q = offset + Vec3::Y * 0.3;
    let exact = winding_number_exact(&moved, q);
    assert!((exact - 1.0).abs() < 1e-4, "{exact}");
    // Within the beta = 2 error measured near the origin (0.041 on a sphere).
    let w = tree.winding_number(q);
    assert!((w - exact).abs() < 0.06, "{w} vs {exact}");
}

#[test]
fn tree_intersection_under_a_scaling_transform() {
    let a = TriBvh::build(cube()).expect("a");
    let b = TriBvh::build(cube()).expect("b");
    // b scaled to a tenth and placed inside a: no contact; moved onto a's face: contact.
    let inside =
        Affine3A::from_scale_rotation_translation(Vec3::splat(0.1), Quat::IDENTITY, Vec3::splat(0.45));
    assert!(!a.intersects(&b, Some(&inside)));
    let on_face = Affine3A::from_scale_rotation_translation(
        Vec3::splat(0.1),
        Quat::IDENTITY,
        Vec3::new(0.45, 0.45, 0.95),
    );
    assert!(a.intersects(&b, Some(&on_face)));
    // Scaled up 3x around a: a is inside b, surfaces apart.
    let around =
        Affine3A::from_scale_rotation_translation(Vec3::splat(3.0), Quat::IDENTITY, Vec3::splat(-1.0));
    assert!(!a.intersects(&b, Some(&around)));
}

// ---- point hash --------------------------------------------------------------------------

#[test]
fn point_hash_queries_match_brute_force() {
    let mut rng = Rng::new(42);
    let points: Vec<Vec3> = (0..2000).map(|_| random_point(&mut rng, 5.0)).collect();
    for cell in [0.25, 1.0, 3.0] {
        let hash = PointHash::build(&points, cell).expect("hash");
        assert_eq!(hash.len(), points.len());
        for _ in 0..100 {
            let c = random_point(&mut rng, 6.0);
            let r = rng.range(0.0..2.5);
            let want: Vec<u32> = (0..points.len() as u32)
                .filter(|&i| points[i as usize].distance_squared(c) <= r * r)
                .collect();
            assert_eq!(hash.within_radius(c, r), want, "cell {cell} r {r}");
            let near = hash.nearest(c, f32::INFINITY).expect("nearest");
            let best = points.iter().map(|p| p.distance(c)).fold(f32::INFINITY, f32::min);
            assert!((near.1 - best).abs() < 1e-6, "{} vs {best}", near.1);
            assert!((points[near.0 as usize].distance(c) - near.1).abs() < 1e-6);
            match hash.nearest(c, r) {
                Some((_, d)) => assert!(d <= r && (d - best).abs() < 1e-6),
                None => assert!(best > r),
            }
        }
        // A ball holding everything takes the occupied-cell path.
        assert_eq!(hash.within_radius(Vec3::ZERO, 100.0).len(), points.len());
    }
}

/// audit-v034b-numerics M2: the cell box rebuilt from the key as `k * cell_size` did not always
/// contain the points bucketed into that cell (f32 rounding of `p * (1 / cell_size)`), and a
/// saturated key's box was nowhere near its points, so the cull skipped a cell holding the
/// query point itself.
#[test]
fn point_hash_radius_query_finds_a_point_at_its_own_position() {
    let p = Vec3::new(-1_563.756_2, 0.0, 0.0);
    let hash = PointHash::build(&[p], 0.028_250_104).expect("hash");
    assert_eq!(hash.within_radius(p, 0.0), vec![0], "rounding at a cell edge");
    assert_eq!(hash.within_radius(p, 1e-5), vec![0]);
    let far = Vec3::new(3.0e6, 0.0, 0.0);
    let saturated = PointHash::build(&[far], 0.001).expect("hash");
    assert_eq!(saturated.within_radius(far, 0.0), vec![0], "saturated key");
    assert_eq!(saturated.within_radius(far, 10.0), vec![0]);
    // A sweep of points near cell edges: every point finds itself at radius 0.
    let mut rng = Rng::new(0x5eed);
    for _ in 0..2000 {
        let cell = rng.range(0.001_f32..0.1);
        let q = Vec3::new(
            rng.range(-5000.0_f32..5000.0),
            rng.range(-5000.0_f32..5000.0),
            rng.range(-5000.0_f32..5000.0),
        );
        let h = PointHash::build(&[q], cell).expect("hash");
        assert_eq!(h.within_radius(q, 0.0), vec![0], "{q} in cells of {cell}");
    }
}

#[test]
fn point_hash_edge_cases() {
    let empty = PointHash::build(&[], 1.0).expect("empty");
    assert!(empty.is_empty());
    assert!(empty.within_radius(Vec3::ZERO, 10.0).is_empty());
    assert!(empty.nearest(Vec3::ZERO, f32::INFINITY).is_none());
    let pts = [Vec3::ZERO, Vec3::ZERO, Vec3::X, Vec3::new(-1e6, 0.0, 0.0)];
    let hash = PointHash::build(&pts, 0.5).expect("hash");
    assert_eq!(
        hash.within_radius(Vec3::ZERO, 0.0),
        vec![0, 1],
        "duplicates and r = 0"
    );
    assert_eq!(
        hash.within_radius(Vec3::ZERO, 1.0),
        vec![0, 1, 2],
        "on the sphere counts"
    );
    assert!(hash.within_radius(Vec3::ZERO, -1.0).is_empty());
    let tie = hash
        .nearest(Vec3::new(0.1, 0.0, 0.0), f32::INFINITY)
        .expect("tie");
    assert_eq!(tie.0, 0, "ties go to the lower index");
    assert!((tie.1 - 0.1).abs() < 1e-6);
    // A far, isolated point is found by the scan fallback.
    let far = hash
        .nearest(Vec3::new(-2e6, 0.0, 0.0), f32::INFINITY)
        .expect("far");
    assert_eq!(far.0, 3);
    assert_eq!(hash.occupied_cells(), 3);
}

// ---- benchmark ---------------------------------------------------------------------------

/// Build and query times for a 100k-triangle mesh. Prints only; nothing is timed in an assert.
/// `cargo test --release -p regia_core --test mesh_query -- --ignored --nocapture bench_100k`
#[test]
#[ignore = "benchmark: run by hand, see the ADR-1461 lane report"]
fn bench_100k_triangles() {
    use std::time::Instant;
    let mesh = uv_sphere(1.0, 250, 201);
    println!("triangles: {}", mesh.triangle_count());
    let t = Instant::now();
    let bvh = TriBvh::build(mesh.clone()).expect("bvh");
    println!(
        "TriBvh::build: {:.1} ms ({} nodes)",
        t.elapsed().as_secs_f64() * 1e3,
        bvh.node_count()
    );
    let mut rng = Rng::new(5);
    let queries: Vec<Vec3> = (0..100_000).map(|_| random_point(&mut rng, 1.5)).collect();
    // Closest point by where the query is: near the surface, far outside, deep inside (the worst
    // case for box trees: every triangle of a sphere is about equally far from its center).
    for (label, lo, hi) in [
        ("near surface", 0.95_f32, 1.05_f32),
        ("outside 2-3", 2.0, 3.0),
        ("inside < 0.5", 0.0, 0.5),
    ] {
        let points: Vec<Vec3> = (0..10_000)
            .map(|_| rng.unit_vector() * rng.range(lo..hi))
            .collect();
        let t = Instant::now();
        let mut sum = 0.0;
        for &p in &points {
            sum += bvh.closest_point(p, f32::INFINITY).map_or(0.0, |h| h.distance);
        }
        println!(
            "closest_point x10k {label}: {:.1} ms (checksum {sum:.3})",
            t.elapsed().as_secs_f64() * 1e3
        );
    }
    let rays: Vec<Ray> = (0..100_000)
        .map(|_| Ray::new(random_point(&mut rng, 1.5), rng.unit_vector()))
        .collect();
    let t = Instant::now();
    let mut hits = 0;
    for ray in &rays {
        hits += usize::from(bvh.ray_cast(ray, f32::INFINITY).is_some());
    }
    println!(
        "ray_cast x100k: {:.1} ms ({hits} hits)",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    let tree = WindingNumberTree::from_bvh(bvh, 2.0).expect("tree");
    println!(
        "WindingNumberTree::from_bvh: {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    let mut inside = 0;
    for &p in &queries[..10_000] {
        inside += usize::from(tree.is_inside(p));
    }
    println!(
        "winding_number x10k: {:.1} ms ({inside} inside)",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    let mut exact = 0.0;
    for &p in &queries[..100] {
        exact += winding_number_exact(&mesh, p);
    }
    println!(
        "winding_number_exact x100: {:.1} ms (sum {exact:.2})",
        t.elapsed().as_secs_f64() * 1e3
    );
    for h in [0.05, 0.025] {
        let t = Instant::now();
        let sdf = SignedDistanceGrid::build(&tree, &SdfParams::new(h)).expect("sdf");
        println!(
            "SignedDistanceGrid h={h}: {:.1} ms ({} points)",
            t.elapsed().as_secs_f64() * 1e3,
            sdf.grid().len()
        );
    }
    let t = Instant::now();
    let hash = PointHash::build(&queries, 0.05).expect("hash");
    let mut found = 0;
    for &p in &queries[..10_000] {
        found += hash.within_radius(p, 0.05).len();
    }
    println!(
        "PointHash build 100k + within_radius x10k: {:.1} ms ({found} found)",
        t.elapsed().as_secs_f64() * 1e3
    );
}
