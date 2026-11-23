// SPDX-License-Identifier: MPL-2.0
//! Degenerate inputs to the pick casts: NaN and zero rays, empty and malformed meshes, zero-scale
//! models, empty worlds and odd distance limits. None of them may panic or return a bogus hit.

use std::sync::Arc;

use regia_core::math::{Mat4, Ray, Vec3};
use regia_ecs::prelude::*;
use regia_pick::{
    BVH_MIN_TRIANGLES, FaceCull, LayerSet, MeshBvh, PickFilter, bvh_for, ray_meets_box, ray_triangle,
    raycast_mesh, raycast_mesh_bvh, raycast_terrain, world_pick, world_pick_layers,
};
use regia_render::{CpuMesh, box_mesh};

fn cube() -> CpuMesh {
    let (vertices, indices) = box_mesh([1.0, 1.0, 1.0]);
    CpuMesh { vertices, indices }
}

const A: Vec3 = Vec3::new(-1.0, -1.0, 0.0);
const B: Vec3 = Vec3::new(1.0, -1.0, 0.0);
const C: Vec3 = Vec3::new(0.0, 1.0, 0.0);

#[test]
fn ray_triangle_rejects_degenerate_inputs() {
    let origin = Vec3::new(0.0, 0.0, 5.0);
    let toward = Vec3::NEG_Z;
    assert!(ray_triangle(origin, toward, A, B, C, FaceCull::None).is_some());
    // A zero direction, a NaN origin and a NaN direction never hit.
    assert!(ray_triangle(origin, Vec3::ZERO, A, B, C, FaceCull::None).is_none());
    assert!(ray_triangle(Vec3::NAN, toward, A, B, C, FaceCull::None).is_none());
    assert!(ray_triangle(origin, Vec3::NAN, A, B, C, FaceCull::None).is_none());
    // A zero-area triangle (collinear and coincident vertices) never hits.
    assert!(ray_triangle(origin, toward, A, A, A, FaceCull::None).is_none());
    assert!(ray_triangle(origin, toward, A, B, Vec3::new(0.0, -1.0, 0.0), FaceCull::None).is_none());
    // A triangle behind the ray origin never hits.
    assert!(ray_triangle(origin, Vec3::Z, A, B, C, FaceCull::None).is_none());
    // A ray parallel to the triangle's plane never hits.
    assert!(ray_triangle(origin, Vec3::X, A, B, C, FaceCull::None).is_none());
}

#[test]
fn ray_triangle_culls_by_winding() {
    let origin = Vec3::new(0.0, 0.0, 5.0);
    let front = ray_triangle(origin, Vec3::NEG_Z, A, B, C, FaceCull::Back).expect("front hit");
    assert!(front.front_face);
    // The same triangle seen from behind is a back face.
    let behind = Vec3::new(0.0, 0.0, -5.0);
    assert!(ray_triangle(behind, Vec3::Z, A, B, C, FaceCull::Back).is_none());
    let back = ray_triangle(behind, Vec3::Z, A, B, C, FaceCull::None).expect("back hit");
    assert!(!back.front_face);
}

#[test]
fn face_cull_names() {
    assert_eq!(FaceCull::from_name("none"), Some(FaceCull::None));
    assert_eq!(FaceCull::from_name("back"), Some(FaceCull::Back));
    assert_eq!(FaceCull::from_name(""), None);
    assert_eq!(FaceCull::from_name("Back"), None);
}

#[test]
fn raycast_mesh_rejects_bad_rays_and_models() {
    let mesh = cube();
    let from = Vec3::new(0.0, 0.0, 5.0);
    let cast = |model: Mat4, origin: Vec3, dir: Vec3| {
        raycast_mesh(&mesh, &mesh.indices, model, &[], origin, dir, FaceCull::None)
    };
    assert!(cast(Mat4::IDENTITY, from, Vec3::NEG_Z).is_some());
    assert!(cast(Mat4::IDENTITY, from, Vec3::ZERO).is_none());
    assert!(cast(Mat4::IDENTITY, from, Vec3::NAN).is_none());
    assert!(cast(Mat4::IDENTITY, Vec3::NAN, Vec3::NEG_Z).is_none());
    assert!(cast(Mat4::IDENTITY, Vec3::INFINITY, Vec3::NEG_Z).is_none());
    assert!(cast(Mat4::from_scale(Vec3::ZERO), from, Vec3::NEG_Z).is_none());
    assert!(cast(Mat4::from_scale(Vec3::new(1.0, 0.0, 1.0)), from, Vec3::NEG_Z).is_none());
    assert!(cast(Mat4::from_scale(Vec3::splat(f32::NAN)), from, Vec3::NEG_Z).is_none());
}

#[test]
fn raycast_mesh_handles_empty_and_malformed_index_lists() {
    let mesh = cube();
    let from = Vec3::new(0.0, 0.0, 5.0);
    let cast = |indices: &[u32]| {
        raycast_mesh(
            &mesh,
            indices,
            Mat4::IDENTITY,
            &[],
            from,
            Vec3::NEG_Z,
            FaceCull::None,
        )
    };
    assert!(cast(&[]).is_none());
    // A trailing partial triangle is ignored; an out-of-range index skips only its triangle.
    assert!(cast(&mesh.indices[..2]).is_none());
    let mut indices = mesh.indices.clone();
    indices.extend([0, 1]);
    assert!(cast(&indices).is_some());
    let mut bad = mesh.indices.clone();
    bad.fill(u32::MAX);
    assert!(cast(&bad).is_none());
    let empty = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    assert!(
        raycast_mesh(
            &empty,
            &[0, 1, 2],
            Mat4::IDENTITY,
            &[],
            from,
            Vec3::NEG_Z,
            FaceCull::None
        )
        .is_none()
    );
}

#[test]
fn hit_normals_are_unit_and_distance_is_along_the_ray() {
    let mesh = cube();
    let hit = raycast_mesh(
        &mesh,
        &mesh.indices,
        Mat4::IDENTITY,
        &[],
        Vec3::new(0.1, 0.1, 5.0),
        Vec3::new(0.0, 0.0, -3.0),
        FaceCull::None,
    )
    .expect("hit");
    assert!((hit.normal.length() - 1.0).abs() < 1e-4);
    assert!((hit.smooth_normal.length() - 1.0).abs() < 1e-4);
    assert!((hit.distance - 4.0).abs() < 1e-4, "{}", hit.distance);
    assert!((hit.barycentric.element_sum() - 1.0).abs() < 1e-4);
}

#[test]
fn bvh_build_skips_unusable_triangles() {
    let empty = CpuMesh {
        vertices: Vec::new(),
        indices: Vec::new(),
    };
    assert!(MeshBvh::build(&empty).is_none());

    let mut all_bad = cube();
    all_bad.indices.fill(u32::MAX);
    assert!(MeshBvh::build(&all_bad).is_none());

    // Every vertex NaN: no usable triangle.
    let mut nan = cube();
    for v in &mut nan.vertices {
        v.position = [f32::NAN; 3];
    }
    assert!(MeshBvh::build(&nan).is_none());

    // One NaN vertex drops only the triangles that use it; the rest stay castable.
    let mut some = cube();
    some.vertices[0].position = [f32::NAN; 3];
    assert!(MeshBvh::build(&some).is_some(), "other triangles remain");
}

#[test]
fn bvh_cast_rejects_bad_rays_like_the_brute_force_cast() {
    let mesh = cube();
    let bvh = MeshBvh::build(&mesh).expect("bvh");
    let from = Vec3::new(0.0, 0.0, 5.0);
    let cast = |model: Mat4, origin: Vec3, dir: Vec3| {
        raycast_mesh_bvh(&mesh, &bvh, model, origin, dir, FaceCull::None)
    };
    let brute = raycast_mesh(
        &mesh,
        &mesh.indices,
        Mat4::IDENTITY,
        &[],
        from,
        Vec3::NEG_Z,
        FaceCull::None,
    )
    .expect("brute");
    let fast = cast(Mat4::IDENTITY, from, Vec3::NEG_Z).expect("bvh");
    assert_eq!(brute.triangle, fast.triangle);
    assert!((brute.distance - fast.distance).abs() < 1e-5);
    assert!(cast(Mat4::IDENTITY, from, Vec3::ZERO).is_none());
    assert!(cast(Mat4::IDENTITY, Vec3::NAN, Vec3::NEG_Z).is_none());
    assert!(cast(Mat4::from_scale(Vec3::ZERO), from, Vec3::NEG_Z).is_none());
    // A ray axis-parallel to the mesh and outside it misses.
    assert!(cast(Mat4::IDENTITY, Vec3::new(5.0, 0.0, 5.0), Vec3::NEG_Z).is_none());
    // A ray along a slab boundary axis (zero component) still hits from inside the slab.
    assert!(cast(Mat4::IDENTITY, Vec3::new(0.2, 0.2, 5.0), Vec3::NEG_Z).is_some());
}

#[test]
fn bvh_for_returns_none_below_the_minimum_triangle_count() {
    let small = Arc::new(cube());
    assert!(small.indices.len() / 3 < BVH_MIN_TRIANGLES);
    assert!(bvh_for(&small).is_none());

    // A big mesh of all-invalid triangles caches "no tree" and keeps answering None.
    let big = Arc::new(CpuMesh {
        vertices: Vec::new(),
        indices: vec![0; BVH_MIN_TRIANGLES * 3],
    });
    assert!(bvh_for(&big).is_none());
    assert!(bvh_for(&big).is_none());
}

#[test]
fn ray_meets_box_handles_axis_parallel_and_inverted_boxes() {
    let unit = (Vec3::splat(-1.0), Vec3::splat(1.0));
    assert!(ray_meets_box(Vec3::new(0.0, 0.0, 5.0), Vec3::NEG_Z, unit));
    // Parallel to z and outside the x slab.
    assert!(!ray_meets_box(Vec3::new(3.0, 0.0, 5.0), Vec3::NEG_Z, unit));
    // Pointing away.
    assert!(!ray_meets_box(Vec3::new(0.0, 0.0, 5.0), Vec3::Z, unit));
    // Starting inside always meets.
    assert!(ray_meets_box(Vec3::ZERO, Vec3::X, unit));
    // A flat (zero-thickness) box is hit, thanks to the padding.
    let flat = (Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, 1.0));
    assert!(ray_meets_box(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, flat));
}

#[test]
fn layer_set_covers_every_byte_value() {
    let mut set = LayerSet::NONE;
    assert!(set.layers().is_empty());
    for layer in [0_u8, 63, 64, 127, 128, 191, 192, 255] {
        assert!(!set.contains(layer));
        set.insert(layer);
        assert!(set.contains(layer));
    }
    assert_eq!(set.layers(), vec![0, 63, 64, 127, 128, 191, 192, 255]);
    assert_eq!(LayerSet::ALL.layers().len(), 256);
    assert_eq!(LayerSet::only(&[]), LayerSet::NONE);
    assert_eq!(LayerSet::only(&[7, 7]), LayerSet::only(&[7]));
    assert_eq!(LayerSet::default(), LayerSet::ALL);
}

#[test]
fn picks_on_an_empty_world_miss() {
    let world = World::new();
    let ray = Ray::new(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y);
    assert!(world_pick(&world, ray, &PickFilter::default()).is_none());
    assert!(world_pick_layers(&world, ray, &PickFilter::default()).is_empty());
    assert!(raycast_terrain(&world, Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, f32::INFINITY).is_none());
}

#[test]
fn terrain_cast_rejects_bad_rays_without_a_terrain() {
    let world = World::new();
    assert!(raycast_terrain(&world, Vec3::NAN, Vec3::NEG_Y, 100.0).is_none());
    assert!(raycast_terrain(&world, Vec3::ZERO, Vec3::ZERO, 100.0).is_none());
    assert!(raycast_terrain(&world, Vec3::ZERO, Vec3::NAN, 100.0).is_none());
    assert!(raycast_terrain(&world, Vec3::ZERO, Vec3::NEG_Y, f32::NAN).is_none());
}

#[test]
fn a_filter_with_no_layers_or_no_surfaces_hits_nothing() {
    let world = World::new();
    let ray = Ray::new(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y);
    let none = PickFilter {
        layers: LayerSet::NONE,
        ..PickFilter::default()
    };
    assert!(world_pick(&world, ray, &none).is_none());
    let nothing = PickFilter {
        terrain: false,
        meshes: false,
        ..PickFilter::default()
    };
    assert!(world_pick(&world, ray, &nothing).is_none());
}
