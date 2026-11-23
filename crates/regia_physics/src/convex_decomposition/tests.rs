// SPDX-License-Identifier: MPL-2.0
//! Behavior of `decompose` on closed, open, inverted, degenerate and multi-part meshes.

use std::collections::HashSet;

use regia_core::math::Quat;

use super::*;

fn box_parts(min: Vec3, max: Vec3, positions: &mut Vec<Vec3>, triangles: &mut Vec<[u32; 3]>) {
    let base = positions.len() as u32;
    for z in [min.z, max.z] {
        for y in [min.y, max.y] {
            for x in [min.x, max.x] {
                positions.push(Vec3::new(x, y, z));
            }
        }
    }
    // Corner k has x = bit 0, y = bit 1, z = bit 2; faces counter-clockwise from outside.
    let faces = [
        [0, 2, 3, 1], // -z
        [4, 5, 7, 6], // +z
        [0, 1, 5, 4], // -y
        [2, 6, 7, 3], // +y
        [0, 4, 6, 2], // -x
        [1, 3, 7, 5], // +x
    ];
    for [a, b, c, d] in faces {
        triangles.push([base + a, base + b, base + c]);
        triangles.push([base + a, base + c, base + d]);
    }
}

fn box_mesh(min: Vec3, max: Vec3) -> TriMesh {
    let (mut positions, mut triangles) = (Vec::new(), Vec::new());
    box_parts(min, max, &mut positions, &mut triangles);
    TriMesh::new(positions, triangles).expect("box")
}

/// The signed volume of a closed mesh (positive when its triangles face outward).
fn signed_volume(mesh: &TriMesh) -> f64 {
    let p = mesh.positions();
    mesh.triangles()
        .iter()
        .map(|&[a, b, c]| {
            let (a, b, c) = (
                p[a as usize].as_dvec3(),
                p[b as usize].as_dvec3(),
                p[c as usize].as_dvec3(),
            );
            a.dot(b.cross(c)) / 6.0
        })
        .sum()
}

/// An L-shaped prism: the outline (0,0) (2,0) (2,1) (1,1) (1,2) (0,2) extruded from z = 0 to 1
/// (volume 3).
fn l_shape() -> TriMesh {
    let outline = [
        [0.0, 0.0],
        [2.0, 0.0],
        [2.0, 1.0],
        [1.0, 1.0],
        [1.0, 2.0],
        [0.0, 2.0],
    ];
    let n = outline.len() as u32;
    let mut positions = Vec::new();
    for z in [0.0, 1.0] {
        for [x, y] in outline {
            positions.push(Vec3::new(x, y, z));
        }
    }
    let mut triangles = Vec::new();
    // A fan from (0,0) covers the L (it sees every corner).
    for i in 1..n - 1 {
        triangles.push([n, n + i, n + i + 1]); // top, counter-clockwise from +z
        triangles.push([0, i + 1, i]); // bottom, reversed
    }
    for i in 0..n {
        let j = (i + 1) % n;
        triangles.push([i, j, n + j]);
        triangles.push([i, n + j, n + i]);
    }
    let mesh = TriMesh::new(positions, triangles).expect("L");
    assert!((signed_volume(&mesh) - 3.0).abs() < 1e-6);
    mesh
}

/// A latitude/longitude sphere with outward triangles.
fn sphere_parts(center: Vec3, radius: f32, positions: &mut Vec<Vec3>, triangles: &mut Vec<[u32; 3]>) {
    const RINGS: u32 = 12;
    const SEGMENTS: u32 = 16;
    let base = positions.len() as u32;
    positions.push(center + Vec3::Y * radius);
    for r in 1..RINGS {
        let theta = std::f32::consts::PI * r as f32 / RINGS as f32;
        for s in 0..SEGMENTS {
            let phi = std::f32::consts::TAU * s as f32 / SEGMENTS as f32;
            positions.push(
                center + Vec3::new(theta.sin() * phi.cos(), theta.cos(), theta.sin() * phi.sin()) * radius,
            );
        }
    }
    positions.push(center - Vec3::Y * radius);
    let bottom = base + 1 + (RINGS - 1) * SEGMENTS;
    let v = |r: u32, s: u32| base + 1 + (r - 1) * SEGMENTS + s % SEGMENTS;
    for s in 0..SEGMENTS {
        triangles.push([base, v(1, s + 1), v(1, s)]);
        for r in 1..RINGS - 1 {
            triangles.push([v(r, s), v(r, s + 1), v(r + 1, s + 1)]);
            triangles.push([v(r, s), v(r + 1, s + 1), v(r + 1, s)]);
        }
        triangles.push([bottom, v(RINGS - 1, s), v(RINGS - 1, s + 1)]);
    }
}

fn two_spheres() -> TriMesh {
    let (mut positions, mut triangles) = (Vec::new(), Vec::new());
    sphere_parts(Vec3::new(-2.0, 0.0, 0.0), 1.0, &mut positions, &mut triangles);
    sphere_parts(Vec3::new(2.0, 0.0, 0.0), 1.0, &mut positions, &mut triangles);
    let mesh = TriMesh::new(positions, triangles).expect("spheres");
    assert!(signed_volume(&mesh) > 0.0, "sphere triangles must face outward");
    mesh
}

/// A comb: a 5 x 1 x 1 bar with three 1 x 2 x 1 teeth on top, as four touching boxes (volume 11).
fn comb() -> TriMesh {
    let (mut positions, mut triangles) = (Vec::new(), Vec::new());
    box_parts(
        Vec3::ZERO,
        Vec3::new(5.0, 1.0, 1.0),
        &mut positions,
        &mut triangles,
    );
    for x in [0.0, 2.0, 4.0] {
        box_parts(
            Vec3::new(x, 1.0, 0.0),
            Vec3::new(x + 1.0, 3.0, 1.0),
            &mut positions,
            &mut triangles,
        );
    }
    TriMesh::new(positions, triangles).expect("comb")
}

fn total_volume(hulls: &[ConvexHull]) -> f32 {
    hulls.iter().map(ConvexHull::volume).sum()
}

/// Every hull is closed (each directed edge has its twin), encloses volume, and has every vertex
/// on or behind every face plane.
fn assert_valid_hulls(hulls: &[ConvexHull]) {
    for (h, hull) in hulls.iter().enumerate() {
        assert!(
            hull.vertices.len() >= 4,
            "hull {h} has {} vertices",
            hull.vertices.len()
        );
        assert!(hull.volume() > 0.0, "hull {h} has volume {}", hull.volume());
        let mut edges = HashSet::new();
        for &[a, b, c] in &hull.triangles {
            for e in [(a, b), (b, c), (c, a)] {
                assert!(edges.insert(e), "hull {h}: edge {e:?} used twice");
            }
        }
        for &(a, b) in &edges {
            assert!(edges.contains(&(b, a)), "hull {h}: edge ({a}, {b}) has no twin");
        }
        for &v in &hull.vertices {
            assert!(hull.contains(v, 1e-4), "hull {h} is not convex at {v}");
        }
    }
}

fn assert_contains_all(hull: &ConvexHull, points: &[Vec3]) {
    for &p in points {
        assert!(hull.contains(p, 1e-4), "vertex {p} is outside the hull");
    }
}

fn unlimited() -> DecompositionParams {
    DecompositionParams {
        max_hull_vertices: 0,
        ..DecompositionParams::default()
    }
}

#[test]
fn cube_yields_one_hull() {
    let mesh = box_mesh(Vec3::splat(-1.0), Vec3::splat(1.0));
    let hulls = decompose(&mesh, &DecompositionParams::default()).expect("decompose");
    assert_eq!(hulls.len(), 1);
    assert_eq!(hulls[0].vertices.len(), 8);
    assert_eq!(hulls[0].triangles.len(), 12);
    assert!(
        (hulls[0].volume() - 8.0).abs() < 1e-4,
        "volume {}",
        hulls[0].volume()
    );
    assert_contains_all(&hulls[0], mesh.positions());
    assert_valid_hulls(&hulls);
}

#[test]
fn rotated_offset_cube_yields_one_hull() {
    let rotation = Quat::from_euler(regia_core::math::EulerRot::XYZ, 0.4, 0.7, -0.3);
    let offset = Vec3::new(10.0, -3.0, 5.0);
    let base = box_mesh(Vec3::splat(-1.0), Vec3::splat(1.0));
    let positions = base.positions().iter().map(|&p| rotation * p + offset).collect();
    let mesh = TriMesh::new(positions, base.triangles().to_vec()).expect("mesh");
    let hulls = decompose(&mesh, &DecompositionParams::default()).expect("decompose");
    assert_eq!(hulls.len(), 1);
    assert!(
        (hulls[0].volume() - 8.0).abs() < 1e-3,
        "volume {}",
        hulls[0].volume()
    );
    assert_contains_all(&hulls[0], mesh.positions());
}

#[test]
fn l_shape_yields_two_hulls_with_total_volume_within_five_percent() {
    let mesh = l_shape();
    let hulls = decompose(&mesh, &DecompositionParams::default()).expect("decompose");
    assert_eq!(
        hulls.len(),
        2,
        "hull volumes {:?}",
        hulls.iter().map(ConvexHull::volume).collect::<Vec<_>>()
    );
    let total = total_volume(&hulls);
    assert!((total - 3.0).abs() <= 0.05 * 3.0, "total volume {total}");
    // One arm holds the corner square (volume 2), the other the remaining arm (volume 1).
    assert!(
        (hulls[0].volume() - 2.0).abs() < 0.1,
        "first {}",
        hulls[0].volume()
    );
    assert!(
        (hulls[1].volume() - 1.0).abs() < 0.1,
        "second {}",
        hulls[1].volume()
    );
    assert_valid_hulls(&hulls);
    // Together the hulls hold every vertex of the L.
    for &p in mesh.positions() {
        assert!(
            hulls.iter().any(|h| h.contains(p, 1e-4)),
            "vertex {p} is in no hull"
        );
    }
}

#[test]
fn l_shape_with_inverted_winding_or_far_offset_still_yields_two_hulls() {
    let mut flipped = l_shape();
    flipped.flip();
    let hulls = decompose(&flipped, &DecompositionParams::default()).expect("decompose");
    assert_eq!(hulls.len(), 2);
    assert!((total_volume(&hulls) - 3.0).abs() <= 0.15);

    let base = l_shape();
    let offset = Vec3::new(100.0, 50.0, -20.0);
    let positions = base.positions().iter().map(|&p| p + offset).collect();
    let moved = TriMesh::new(positions, base.triangles().to_vec()).expect("mesh");
    let hulls = decompose(&moved, &DecompositionParams::default()).expect("decompose");
    assert_eq!(hulls.len(), 2);
    assert!((total_volume(&hulls) - 3.0).abs() <= 0.15);
}

/// At x = 1e6 the f32 ulp is 0.0625, larger than a resolution-64 cell of this 2 m prism
/// (h = 0.03125). `mark_surface` does round each cell box through `Vec3` before the overlap
/// test, and the audit predicted that would collapse the shell and return one hull. This
/// prism still splits into two hulls with the concave corner left empty.
#[test]
fn l_shape_a_million_metres_out_still_yields_two_hulls() {
    let base = l_shape();
    let offset = Vec3::new(1_000_000.0, 0.0, 0.0);
    let positions = base.positions().iter().map(|&p| p + offset).collect();
    let moved = TriMesh::new(positions, base.triangles().to_vec()).expect("mesh");
    let hulls = decompose(&moved, &DecompositionParams::default()).expect("decompose");
    assert_eq!(
        hulls.len(),
        2,
        "hull volumes {:?}",
        hulls.iter().map(ConvexHull::volume).collect::<Vec<_>>()
    );
    let total = total_volume(&hulls);
    assert!(
        (total - 3.0).abs() <= 0.15,
        "total volume {total} (the concave corner must stay empty)"
    );
    for &p in moved.positions() {
        assert!(
            hulls.iter().any(|h| h.contains(p, 1e-3)),
            "vertex {p} is in no hull"
        );
    }
}

#[test]
fn two_separated_spheres_yield_two_hulls() {
    let mesh = two_spheres();
    let hulls = decompose(&mesh, &unlimited()).expect("decompose");
    assert_eq!(
        hulls.len(),
        2,
        "hull volumes {:?}",
        hulls.iter().map(ConvexHull::volume).collect::<Vec<_>>()
    );
    let (left, right) = if hulls[0].centroid().x < 0.0 {
        (&hulls[0], &hulls[1])
    } else {
        (&hulls[1], &hulls[0])
    };
    assert!((left.centroid() - Vec3::new(-2.0, 0.0, 0.0)).length() < 0.05);
    assert!((right.centroid() - Vec3::new(2.0, 0.0, 0.0)).length() < 0.05);
    let (l_points, r_points): (Vec<Vec3>, Vec<Vec3>) = mesh.positions().iter().partition(|p| p.x < 0.0);
    assert_contains_all(left, &l_points);
    assert_contains_all(right, &r_points);
    // Each hull is its sphere's polyhedron: the hull volumes add up to the mesh's volume.
    let mesh_volume = signed_volume(&mesh) as f32;
    assert!((total_volume(&hulls) - mesh_volume).abs() <= 0.01 * mesh_volume);
    assert_valid_hulls(&hulls);
}

#[test]
fn one_hull_contains_every_vertex() {
    let params = DecompositionParams {
        max_hulls: 1,
        ..unlimited()
    };
    for mesh in [l_shape(), two_spheres(), comb()] {
        let hulls = decompose(&mesh, &params).expect("decompose");
        assert_eq!(hulls.len(), 1);
        assert_contains_all(&hulls[0], mesh.positions());
        // Its vertices are mesh vertices.
        for v in &hulls[0].vertices {
            assert!(mesh.positions().contains(v), "{v} is not a mesh vertex");
        }
        assert_valid_hulls(&hulls);
    }
    // The L's hull is the L plus the triangle prism across its notch: 3 + 0.5.
    let hulls = decompose(&l_shape(), &params).expect("decompose");
    assert!((hulls[0].volume() - 3.5).abs() < 1e-5);
}

#[test]
fn hull_vertex_cap_is_respected() {
    let mesh = two_spheres();
    for max_hulls in [1, 2, 4] {
        let params = DecompositionParams {
            max_hulls,
            max_hull_vertices: 24,
            ..DecompositionParams::default()
        };
        let hulls = decompose(&mesh, &params).expect("decompose");
        assert!(!hulls.is_empty() && hulls.len() <= max_hulls as usize);
        for hull in &hulls {
            assert!(hull.vertices.len() <= 24, "{} vertices", hull.vertices.len());
        }
        assert_valid_hulls(&hulls);
    }
}

#[test]
fn hull_count_never_exceeds_max_hulls() {
    let mesh = comb();
    let mut counts = Vec::new();
    for max_hulls in 1..=8 {
        let params = DecompositionParams {
            max_hulls,
            ..DecompositionParams::default()
        };
        let hulls = decompose(&mesh, &params).expect("decompose");
        assert!(
            hulls.len() <= max_hulls as usize,
            "{} hulls for max {max_hulls}",
            hulls.len()
        );
        assert_valid_hulls(&hulls);
        counts.push(hulls.len());
    }
    assert_eq!(counts[0], 1);
    // The comb has three teeth on a bar: given room, it becomes four pieces or more, and the hulls
    // then hug the solid (volume 11) instead of filling the gaps between the teeth (hull 15).
    let params = DecompositionParams {
        max_hulls: 8,
        ..DecompositionParams::default()
    };
    let hulls = decompose(&mesh, &params).expect("decompose");
    assert!(hulls.len() >= 4, "{} hulls", hulls.len());
    let total = total_volume(&hulls);
    assert!((total - 11.0).abs() <= 0.05 * 11.0, "total volume {total}");
    for &p in mesh.positions() {
        assert!(
            hulls.iter().any(|h| h.contains(p, 1e-4)),
            "vertex {p} is in no hull"
        );
    }
}

#[test]
fn no_cuts_when_depth_is_zero_or_concavity_is_allowed() {
    let mesh = l_shape();
    for params in [
        DecompositionParams {
            max_depth: 0,
            ..DecompositionParams::default()
        },
        DecompositionParams {
            max_concavity: 1.0,
            ..DecompositionParams::default()
        },
    ] {
        let hulls = decompose(&mesh, &params).expect("decompose");
        assert_eq!(hulls.len(), 1);
        assert!((hulls[0].volume() - 3.5).abs() < 1e-4);
    }
}

#[test]
fn coarsest_grid_still_finds_the_l() {
    let params = DecompositionParams {
        resolution: MIN_RESOLUTION,
        ..DecompositionParams::default()
    };
    let hulls = decompose(&l_shape(), &params).expect("decompose");
    assert_eq!(hulls.len(), 2);
    assert!((total_volume(&hulls) - 3.0).abs() <= 0.15);
}

#[test]
fn open_mesh_decomposes_as_a_shell() {
    // A box without its top: the flood fill reaches the inside, so the solid is the thin shell.
    let full = box_mesh(Vec3::splat(-1.0), Vec3::splat(1.0));
    let triangles: Vec<[u32; 3]> = full.triangles()[..2]
        .iter()
        .chain(&full.triangles()[4..])
        .copied()
        .collect();
    let open = TriMesh::new(full.positions().to_vec(), triangles).expect("open box");
    let hulls = decompose(&open, &DecompositionParams::default()).expect("decompose");
    assert!(!hulls.is_empty() && hulls.len() <= 16);
    assert_valid_hulls(&hulls);
    for &p in open.positions() {
        assert!(
            hulls.iter().any(|h| h.contains(p, 1e-3)),
            "vertex {p} is in no hull"
        );
    }
}

#[test]
fn empty_mesh_is_an_error() {
    let params = DecompositionParams::default();
    assert_eq!(
        decompose(&TriMesh::default(), &params),
        Err(DecompositionError::EmptyMesh)
    );
    let no_triangles = TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], Vec::new()).expect("mesh");
    assert_eq!(
        decompose(&no_triangles, &params),
        Err(DecompositionError::EmptyMesh)
    );
}

#[test]
fn zero_area_mesh_is_an_error() {
    let params = DecompositionParams::default();
    // Repeated corners, and collinear corners spread over all three axes.
    let points = vec![
        Vec3::ZERO,
        Vec3::splat(1.0),
        Vec3::splat(2.0),
        Vec3::new(0.0, 0.0, 3.0),
    ];
    let mesh = TriMesh::new(points, vec![[0, 1, 2], [0, 0, 3], [3, 3, 3]]).expect("mesh");
    assert_eq!(decompose(&mesh, &params), Err(DecompositionError::ZeroArea));
}

#[test]
fn flat_mesh_is_an_error() {
    let params = DecompositionParams::default();
    // A tilted quad (its bounds have extent on every axis, but it has no volume).
    let quad = vec![
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(1.0, 1.0, 0.0),
        Vec3::new(1.0, 1.0, 1.0),
        Vec3::new(0.0, 0.0, 1.0),
    ];
    let mesh = TriMesh::new(quad, vec![[0, 1, 2], [0, 2, 3]]).expect("quad");
    assert_eq!(decompose(&mesh, &params), Err(DecompositionError::Flat));
    // A single triangle.
    let tri = TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], vec![[0, 1, 2]]).expect("tri");
    assert_eq!(decompose(&tri, &params), Err(DecompositionError::Flat));
    // Unused vertices off the plane do not count.
    let tri = TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z], vec![[0, 1, 2]]).expect("tri");
    assert_eq!(decompose(&tri, &params), Err(DecompositionError::Flat));
}

#[test]
fn out_of_range_parameters_are_errors() {
    let mesh = box_mesh(Vec3::ZERO, Vec3::ONE);
    let d = DecompositionParams::default();
    let cases = [
        (
            DecompositionParams { max_hulls: 0, ..d },
            "max_hulls",
            "bad_max_hulls",
        ),
        (
            DecompositionParams {
                max_hulls: MAX_HULLS + 1,
                ..d
            },
            "max_hulls",
            "bad_max_hulls",
        ),
        (
            DecompositionParams {
                resolution: MIN_RESOLUTION - 1,
                ..d
            },
            "resolution",
            "bad_resolution",
        ),
        (
            DecompositionParams {
                resolution: MAX_RESOLUTION + 1,
                ..d
            },
            "resolution",
            "bad_resolution",
        ),
        (
            DecompositionParams {
                max_concavity: -0.1,
                ..d
            },
            "max_concavity",
            "bad_max_concavity",
        ),
        (
            DecompositionParams {
                max_concavity: 1.5,
                ..d
            },
            "max_concavity",
            "bad_max_concavity",
        ),
        (
            DecompositionParams {
                max_concavity: f32::NAN,
                ..d
            },
            "max_concavity",
            "bad_max_concavity",
        ),
        (
            DecompositionParams {
                max_depth: MAX_DEPTH + 1,
                ..d
            },
            "max_depth",
            "bad_max_depth",
        ),
        (
            DecompositionParams {
                max_hull_vertices: 3,
                ..d
            },
            "max_hull_vertices",
            "bad_max_hull_vertices",
        ),
        (
            DecompositionParams {
                max_hull_vertices: MAX_HULL_VERTICES + 1,
                ..d
            },
            "max_hull_vertices",
            "bad_max_hull_vertices",
        ),
    ];
    for (params, field, code) in cases {
        let err = decompose(&mesh, &params).expect_err(field);
        assert!(
            matches!(err, DecompositionError::InvalidParameter { name, .. } if name == field),
            "{err:?}"
        );
        assert_eq!(err.code(), code);
        assert!(err.to_string().contains(field));
        // Parameters are checked before the mesh.
        assert_eq!(decompose(&TriMesh::default(), &params), Err(err));
    }
    // The edges of every range are accepted.
    for params in [
        DecompositionParams {
            max_hulls: MAX_HULLS,
            max_hull_vertices: 4,
            ..d
        },
        DecompositionParams {
            resolution: MIN_RESOLUTION,
            max_concavity: 0.0,
            max_depth: 0,
            ..d
        },
        DecompositionParams {
            max_hull_vertices: MAX_HULL_VERTICES,
            max_concavity: 1.0,
            ..d
        },
    ] {
        assert_eq!(params.validate(), Ok(()));
        assert!(decompose(&mesh, &params).is_ok());
    }
}

#[test]
fn error_codes_are_protocol_safe() {
    let errors = [
        DecompositionError::EmptyMesh,
        DecompositionError::ZeroArea,
        DecompositionError::Flat,
        DecompositionError::InvalidParameter {
            name: "max_hulls",
            reason: "",
        },
        DecompositionError::InvalidParameter {
            name: "max_hull_vertices",
            reason: "",
        },
    ];
    let codes: Vec<&str> = errors.iter().map(DecompositionError::code).collect();
    assert_eq!(
        codes,
        [
            "empty_mesh",
            "zero_area",
            "flat_mesh",
            "bad_max_hulls",
            "bad_max_hull_vertices"
        ]
    );
    for code in codes {
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
    }
}

#[test]
fn decomposition_is_deterministic() {
    for mesh in [l_shape(), two_spheres(), comb()] {
        let params = DecompositionParams::default();
        let first = decompose(&mesh, &params).expect("decompose");
        let second = decompose(&mesh.clone(), &params).expect("decompose");
        assert_eq!(first, second);
    }
}

#[test]
fn hulls_become_collider_shapes() {
    assert_eq!(compound_shape(&[]), None);
    let cube = decompose(&box_mesh(Vec3::ZERO, Vec3::ONE), &DecompositionParams::default()).expect("cube");
    match compound_shape(&cube) {
        Some(ColliderShape::ConvexHull { points }) => assert_eq!(points, cube[0].vertices),
        other => panic!("expected a convex hull, got {other:?}"),
    }
    let l = decompose(&l_shape(), &DecompositionParams::default()).expect("L");
    match compound_shape(&l) {
        Some(ColliderShape::Compound { shapes }) => {
            assert_eq!(shapes.len(), l.len());
            for ((transform, shape), hull) in shapes.iter().zip(&l) {
                assert_eq!(*transform, Transform::IDENTITY);
                assert_eq!(*shape, hull.to_collider_shape());
            }
        }
        other => panic!("expected a compound, got {other:?}"),
    }
}

#[test]
fn hull_measures() {
    let cube = ConvexHull {
        vertices: box_mesh(Vec3::ZERO, Vec3::new(2.0, 1.0, 1.0))
            .positions()
            .to_vec(),
        triangles: box_mesh(Vec3::ZERO, Vec3::new(2.0, 1.0, 1.0))
            .triangles()
            .to_vec(),
    };
    assert!((cube.volume() - 2.0).abs() < 1e-6);
    assert!((cube.centroid() - Vec3::new(1.0, 0.5, 0.5)).length() < 1e-6);
    assert!(cube.contains(Vec3::new(1.0, 0.5, 0.5), 0.0));
    assert!(cube.contains(Vec3::new(2.0, 1.0, 1.0), 1e-6));
    assert!(!cube.contains(Vec3::new(2.1, 0.5, 0.5), 0.05));
    assert!(cube.contains(Vec3::new(2.1, 0.5, 0.5), 0.2));
    let empty = ConvexHull {
        vertices: Vec::new(),
        triangles: Vec::new(),
    };
    assert_eq!(empty.volume(), 0.0);
    assert_eq!(empty.centroid(), Vec3::ZERO);
}

/// A torus around the y axis (major radius 2, tube radius 0.6), outward triangles.
fn torus(major_segments: u32, minor_segments: u32) -> TriMesh {
    let (major, minor) = (2.0f32, 0.6f32);
    let mut positions = Vec::new();
    for i in 0..major_segments {
        let u = std::f32::consts::TAU * i as f32 / major_segments as f32;
        for j in 0..minor_segments {
            let v = std::f32::consts::TAU * j as f32 / minor_segments as f32;
            let ring = major + minor * v.cos();
            positions.push(Vec3::new(ring * u.cos(), minor * v.sin(), ring * u.sin()));
        }
    }
    let at = |i: u32, j: u32| (i % major_segments) * minor_segments + j % minor_segments;
    let mut triangles = Vec::new();
    for i in 0..major_segments {
        for j in 0..minor_segments {
            triangles.push([at(i, j), at(i, j + 1), at(i + 1, j + 1)]);
            triangles.push([at(i, j), at(i + 1, j + 1), at(i + 1, j)]);
        }
    }
    let mut mesh = TriMesh::new(positions, triangles).expect("torus");
    if signed_volume(&mesh) < 0.0 {
        mesh.flip();
    }
    mesh
}

#[test]
fn torus_hulls_do_not_bridge_the_hole() {
    let mesh = torus(32, 12);
    let mesh_volume = signed_volume(&mesh) as f32;
    let hulls = decompose(&mesh, &DecompositionParams::default()).expect("decompose");
    assert!(
        (4..=16).contains(&hulls.len()),
        "{} hulls, volumes {:?}",
        hulls.len(),
        hulls.iter().map(ConvexHull::volume).collect::<Vec<_>>()
    );
    // No hull reaches the center of the hole, and together the hulls stay close to the tube's
    // volume (the single hull of the torus is about 1.5 times it).
    for hull in &hulls {
        assert!(!hull.contains(Vec3::ZERO, 0.0), "a hull spans the hole");
    }
    let total = total_volume(&hulls);
    assert!(
        total >= 0.95 * mesh_volume && total <= 1.3 * mesh_volume,
        "total {total}, mesh {mesh_volume}"
    );
    let single = decompose(
        &mesh,
        &DecompositionParams {
            max_hulls: 1,
            ..DecompositionParams::default()
        },
    )
    .expect("decompose");
    assert!(single[0].contains(Vec3::ZERO, 0.0));
    assert_valid_hulls(&hulls);
}

/// Two boxes stacked so the notch runs the whole length of the long axis: no cut across the long
/// (x) axis removes it, only one along a short axis (y = 1 or z = 1, both leave two boxes).
#[test]
fn step_is_cut_along_a_short_axis() {
    let (mut positions, mut triangles) = (Vec::new(), Vec::new());
    box_parts(
        Vec3::ZERO,
        Vec3::new(10.0, 1.0, 2.0),
        &mut positions,
        &mut triangles,
    );
    box_parts(
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(10.0, 2.0, 1.0),
        &mut positions,
        &mut triangles,
    );
    let mesh = TriMesh::new(positions, triangles).expect("step");
    let hulls = decompose(&mesh, &DecompositionParams::default()).expect("decompose");
    assert_eq!(hulls.len(), 2);
    let total = total_volume(&hulls);
    assert!((total - 30.0).abs() <= 0.02 * 30.0, "total {total}");
    // Two boxes of volume 20 and 10, each spanning the full length.
    assert!(
        (hulls[0].volume() - 20.0).abs() < 0.3,
        "first {}",
        hulls[0].volume()
    );
    assert!(
        (hulls[1].volume() - 10.0).abs() < 0.3,
        "second {}",
        hulls[1].volume()
    );
    for hull in &hulls {
        let (lo, hi) = hull
            .vertices
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v.x), hi.max(v.x)));
        assert!(lo.abs() < 1e-5 && (hi - 10.0).abs() < 1e-5, "x span {lo}..{hi}");
    }
    assert_valid_hulls(&hulls);
}

/// Prints decomposition times for a dense torus (hand-run: `cargo test -p regia_physics --release
/// -- --ignored --nocapture perf_decompose`). Not part of the gate: it asserts nothing about time.
#[test]
#[ignore = "hand-run timing"]
fn perf_decompose_dense_torus() {
    let mesh = torus(128, 48);
    for resolution in [32, 64, 128] {
        let params = DecompositionParams {
            resolution,
            ..DecompositionParams::default()
        };
        let start = std::time::Instant::now();
        let hulls = decompose(&mesh, &params).expect("decompose");
        println!(
            "torus {} triangles, resolution {resolution}: {} hulls in {:.1} ms",
            mesh.triangle_count(),
            hulls.len(),
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
}
