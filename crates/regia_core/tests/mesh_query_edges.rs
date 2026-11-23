// SPDX-License-Identifier: MPL-2.0
//! `mesh_query` value types and parameter edges not covered by `mesh_query.rs`: mesh construction
//! errors, `flip` and `bounds`, the winding tree's accuracy parameter, and `DenseGrid3` indexing.

use regia_core::glam::{UVec3, Vec3, vec3};
use regia_core::math::Aabb;
use regia_core::mesh_query::{
    DenseGrid3, MeshQueryError, TriBvh, TriMesh, WindingNumberTree, winding_number_exact,
};

/// A tetrahedron with outward-facing triangles.
fn tetra() -> TriMesh {
    let positions = vec![
        vec3(1.0, 1.0, 1.0),
        vec3(1.0, -1.0, -1.0),
        vec3(-1.0, 1.0, -1.0),
        vec3(-1.0, -1.0, 1.0),
    ];
    TriMesh::new(positions, vec![[0, 1, 2], [0, 3, 1], [0, 2, 3], [1, 3, 2]]).expect("tetrahedron")
}

#[test]
fn mesh_construction_errors_are_typed_and_name_the_culprit() {
    let p = vec![Vec3::ZERO, Vec3::X, Vec3::Y];
    assert_eq!(
        TriMesh::from_indices(p.clone(), &[0, 1]),
        Err(MeshQueryError::IndexCount(2))
    );
    assert_eq!(
        TriMesh::from_indices(p.clone(), &[0, 1, 2, 0]),
        Err(MeshQueryError::IndexCount(4))
    );
    assert_eq!(
        TriMesh::from_indices(p.clone(), &[0, 1, 2, 2, 1, 3]),
        Err(MeshQueryError::IndexOutOfRange {
            triangle: 1,
            index: 3,
            vertex_count: 3
        })
    );
    let mut bad = p.clone();
    bad[2] = vec3(0.0, f32::NAN, 0.0);
    assert_eq!(
        TriMesh::new(bad, vec![[0, 1, 2]]),
        Err(MeshQueryError::NonFinite(2))
    );
    let mut inf = p.clone();
    inf[0] = vec3(f32::INFINITY, 0.0, 0.0);
    assert_eq!(
        TriMesh::new(inf, vec![]),
        Err(MeshQueryError::NonFinite(0)),
        "checked even without triangles"
    );
    let empty = TriMesh::from_indices(vec![], &[]).expect("an empty mesh is a valid value");
    assert!(empty.is_empty() && empty.triangle_count() == 0);
    assert!(empty.bounds().is_empty());
    assert_eq!(TriBvh::build(empty).err(), Some(MeshQueryError::EmptyMesh));
    // The error messages carry their numbers.
    let text = MeshQueryError::IndexOutOfRange {
        triangle: 7,
        index: 99,
        vertex_count: 4,
    }
    .to_string();
    assert!(
        text.contains('7') && text.contains("99") && text.contains('4'),
        "{text}"
    );
}

#[test]
fn bounds_cover_only_vertices_that_triangles_use() {
    let positions = vec![Vec3::ZERO, Vec3::X, Vec3::Y, vec3(100.0, 100.0, 100.0)];
    let mesh = TriMesh::new(positions, vec![[0, 1, 2]]).expect("mesh");
    assert_eq!(
        mesh.bounds(),
        Aabb::new(Vec3::ZERO, vec3(1.0, 1.0, 0.0)),
        "the stray vertex is ignored"
    );
    assert_eq!(mesh.positions().len(), 4);
}

#[test]
fn flip_swaps_inside_and_outside_and_twice_is_the_identity() {
    let mesh = tetra();
    let inside = Vec3::ZERO;
    let outside = vec3(5.0, 5.0, 5.0);
    assert!((winding_number_exact(&mesh, inside) - 1.0).abs() < 1e-5);
    assert!(winding_number_exact(&mesh, outside).abs() < 1e-5);
    let mut flipped = mesh.clone();
    flipped.flip();
    assert_ne!(flipped, mesh);
    assert!(
        (winding_number_exact(&flipped, inside) + 1.0).abs() < 1e-5,
        "inside a flipped shell reads -1"
    );
    assert!(winding_number_exact(&flipped, outside).abs() < 1e-5);
    flipped.flip();
    assert_eq!(flipped, mesh);
}

#[test]
fn winding_beta_is_validated_and_a_failed_change_keeps_the_old_value() {
    let mut tree = WindingNumberTree::build(tetra()).expect("tree");
    assert_eq!(tree.beta(), WindingNumberTree::DEFAULT_BETA);
    for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let err = tree.set_beta(bad).expect_err("rejected");
        assert!(matches!(err, MeshQueryError::InvalidBeta(_)), "{bad}");
        assert_eq!(
            tree.beta(),
            WindingNumberTree::DEFAULT_BETA,
            "unchanged after {bad}"
        );
    }
    tree.set_beta(8.0).expect("valid");
    assert_eq!(tree.beta(), 8.0);
    tree.set_beta(f32::MIN_POSITIVE).expect("tiny but positive");
    assert!(WindingNumberTree::from_bvh(TriBvh::build(tetra()).expect("bvh"), 0.0).is_err());
    assert!(WindingNumberTree::from_bvh(TriBvh::build(tetra()).expect("bvh"), f32::NAN).is_err());
}

#[test]
fn winding_answers_agree_with_the_exact_sum_at_any_valid_beta_and_nan_for_nan_points() {
    let mut tree = WindingNumberTree::build(tetra()).expect("tree");
    let probes = [
        Vec3::ZERO,
        vec3(0.2, -0.1, 0.3),
        vec3(3.0, 0.0, 0.0),
        vec3(-4.0, 2.0, 1.0),
    ];
    for beta in [0.5f32, 2.0, 16.0] {
        tree.set_beta(beta).expect("valid");
        for p in probes {
            let exact = winding_number_exact(tree.bvh().mesh(), p);
            let fast = tree.winding_number(p);
            // A four-triangle mesh is one leaf: the tree sums it exactly whatever beta says.
            assert!(
                (fast - exact).abs() < 1e-4,
                "beta {beta} at {p}: {fast} vs {exact}"
            );
        }
    }
    assert!(tree.winding_number(vec3(f32::NAN, 0.0, 0.0)).is_nan());
    assert!(tree.winding_number(vec3(0.0, f32::INFINITY, 0.0)).is_nan());
    assert!(!tree.is_inside(vec3(f32::NAN, 0.0, 0.0)), "NaN is not inside");
    assert!(tree.is_inside(Vec3::ZERO) && !tree.is_inside(vec3(9.0, 9.0, 9.0)));
    let bvh = tree.into_bvh();
    assert_eq!(bvh.mesh().triangle_count(), 4);
}

#[test]
fn dense_grid_indexing_bounds_and_empty_dimensions() {
    let mut g = DenseGrid3::new(UVec3::new(2, 3, 4), 0i32);
    assert_eq!(
        (g.len(), g.dims(), g.is_empty()),
        (24, UVec3::new(2, 3, 4), false)
    );
    *g.get_mut(1, 2, 3).expect("inside") = 9;
    assert_eq!(g[[1, 2, 3]], 9);
    assert_eq!(g.values().last(), Some(&9), "the far corner is the last cell");
    assert_eq!(g.linear(1, 2, 3), 23);
    for outside in [(2, 0, 0), (0, 3, 0), (0, 0, 4), (u32::MAX, 0, 0)] {
        assert_eq!(g.get(outside.0, outside.1, outside.2), None, "{outside:?}");
        assert!(g.get_mut(outside.0, outside.1, outside.2).is_none());
    }
    g.values_mut().fill(5);
    assert!(g.values().iter().all(|&v| v == 5));
    let flat = DenseGrid3::new(UVec3::new(4, 0, 4), 1u8);
    assert!(flat.is_empty());
    assert_eq!(flat.values().len(), 0);
    assert_eq!(flat.get(0, 0, 0), None);
    let clone = g.clone();
    assert_eq!(clone, g);
}

#[test]
#[should_panic(expected = "outside")]
fn dense_grid_index_outside_panics_with_the_cell() {
    let g = DenseGrid3::new(UVec3::new(2, 2, 2), 0u8);
    let _ = g[[0, 2, 0]];
}
