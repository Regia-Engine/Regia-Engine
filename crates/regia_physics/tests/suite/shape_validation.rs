// SPDX-License-Identifier: MPL-2.0
//! Regression tests: invalid shapes must be rejected instead of aborting inside parry.

use regia_core::math::Vec3;
use regia_physics::ColliderShape;
use regia_physics::step::shape_to_shared_shape;
use regia_scene::Transform;

fn tri_mesh(index: u32) -> ColliderShape {
    ColliderShape::TriMesh {
        vertices: vec![Vec3::ZERO, Vec3::X, Vec3::Z],
        indices: vec![[0, 1, index]],
    }
}

#[test]
fn trimesh_index_past_vertices_is_rejected() {
    assert!(shape_to_shared_shape(&tri_mesh(2)).is_some());
    assert!(shape_to_shared_shape(&tri_mesh(5)).is_none());
}

#[test]
fn compound_with_composite_child_is_rejected() {
    let boxed = ColliderShape::Box {
        half_extents: Vec3::ONE,
    };
    let ok = ColliderShape::Compound {
        shapes: vec![(Transform::IDENTITY, boxed.clone())],
    };
    assert!(shape_to_shared_shape(&ok).is_some());
    let with_mesh = ColliderShape::Compound {
        shapes: vec![
            (Transform::IDENTITY, boxed.clone()),
            (Transform::IDENTITY, tri_mesh(2)),
        ],
    };
    assert!(shape_to_shared_shape(&with_mesh).is_none());
    let nested = ColliderShape::Compound {
        shapes: vec![(Transform::IDENTITY, ok)],
    };
    assert!(shape_to_shared_shape(&nested).is_none());
}
