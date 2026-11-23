// SPDX-License-Identifier: MPL-2.0
//! Regression tests for recoverable-path panics found by the 2026-11-02 review: malformed public
//! data must be rejected or ignored, never abort.

use regia_core::math::{Affine3A, IVec2, Vec3};
use regia_physics::ColliderShape;
use regia_physics::cloth::{
    ClothAsset, ClothConfig, ClothSolver, MapEdit, PoseTarget, apply_map_edit, transfer_maps,
};
use regia_physics::convex_decomposition::ConvexHull;
use regia_physics::platformer2d::PlatformerWorld;
use regia_physics::step::shape_to_shared_shape;

fn target(p: Vec3) -> PoseTarget {
    PoseTarget {
        position: p,
        normal: Vec3::Y,
    }
}

#[test]
fn adding_a_particle_after_binding_a_pose_does_not_abort_output_or_step() {
    let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
    solver.add_particle(Vec3::ZERO, 1.0).expect("particle");
    solver
        .set_pose_targets(&[target(Vec3::ZERO)], &[target(Vec3::ZERO)])
        .expect("targets");
    solver.add_particle(Vec3::X, 1.0).expect("particle");
    let mut out = Vec::new();
    solver.output(&mut out);
    assert_eq!(out.len(), 2);
    solver.step(1.0 / 60.0).expect("step");
    solver.output(&mut out);
    assert_eq!(out.len(), 2);
}

fn empty_asset() -> ClothAsset {
    ClothAsset {
        name: String::new(),
        proxies: Vec::new(),
        lods: Vec::new(),
        dirty: false,
    }
}

#[test]
fn an_asset_without_lods_errors_instead_of_indexing() {
    let mut a = empty_asset();
    assert!(a.build_solver(0, 1.0 / 60.0, Affine3A::IDENTITY).is_err());
    assert!(!a.is_skinned());
    assert!(a.lod_transfer(0, 1).is_empty());
    let edit = MapEdit::Set { vertices: vec![] };
    assert!(apply_map_edit(&mut a, 0, "reach", &edit).is_err());
    let mut b = empty_asset();
    transfer_maps(&a, &mut b);
}

fn bad_triangle_asset() -> ClothAsset {
    let mut a = empty_asset();
    a.lods.push(regia_physics::cloth::asset::ClothLod {
        rest_positions: vec![Vec3::ZERO],
        triangles: vec![[0, 5, 5]],
        ..Default::default()
    });
    a
}

#[test]
fn out_of_range_triangles_error_in_build_and_map_edit() {
    let mut a = bad_triangle_asset();
    assert!(a.build_solver(0, 1.0 / 60.0, Affine3A::IDENTITY).is_err());
    let relax = MapEdit::Relax {
        iterations: 1,
        strength: 1.0,
    };
    assert!(apply_map_edit(&mut a, 0, "reach", &relax).is_err());
}

#[test]
fn a_short_map_is_dropped_when_resampling_to_a_different_mesh() {
    let mut from = empty_asset();
    from.lods.push(regia_physics::cloth::asset::ClothLod {
        rest_positions: vec![Vec3::ZERO, Vec3::X, Vec3::new(2.0, 0.0, 0.0)],
        maps: regia_physics::cloth::asset::ClothMaps {
            reach: vec![1.0],
            ..Default::default()
        },
        ..Default::default()
    });
    let mut to = empty_asset();
    to.lods.push(regia_physics::cloth::asset::ClothLod {
        rest_positions: vec![Vec3::new(2.0, 0.0, 0.0), Vec3::new(3.0, 0.0, 0.0)],
        ..Default::default()
    });
    transfer_maps(&from, &mut to);
    assert!(to.lods[0].maps.reach.is_empty());
}

#[test]
fn lod_transfer_from_an_empty_lod_is_empty() {
    let mut a = empty_asset();
    a.lods.push(Default::default());
    a.lods.push(regia_physics::cloth::asset::ClothLod {
        rest_positions: vec![Vec3::ZERO],
        ..Default::default()
    });
    assert!(a.lod_transfer(0, 1).is_empty());
}

#[test]
fn a_hull_with_out_of_range_indices_measures_zero() {
    let hull = ConvexHull {
        vertices: vec![Vec3::ZERO],
        triangles: vec![[0, 1, 2]],
    };
    assert_eq!(hull.volume(), 0.0);
    let _ = hull.centroid();
    assert!(!hull.contains(Vec3::ZERO, 0.1));
}

#[test]
fn negative_zero_cylinder_sizes_are_rejected() {
    let cyl = |half_height, radius| ColliderShape::Cylinder {
        half_height,
        radius,
        axis: regia_physics::ShapeAxis::Y,
    };
    assert!(shape_to_shared_shape(&cyl(1.0, 1.0)).is_some());
    assert!(shape_to_shared_shape(&cyl(0.0, 1.0)).is_some());
    assert!(shape_to_shared_shape(&cyl(-0.0, 1.0)).is_none());
    assert!(shape_to_shared_shape(&cyl(1.0, -0.0)).is_none());
}

#[test]
fn platformer_boxes_near_i32_max_do_not_overflow() {
    let mut world = PlatformerWorld::new();
    world.add_solid(IVec2::ZERO, 8, 8);
    world.add_actor(IVec2::new(1, 0), i32::MAX, 8);
}
