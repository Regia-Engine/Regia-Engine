// SPDX-License-Identifier: MPL-2.0
//! `Draw3d` and `Quick3dCache` through the public API: parameter clamping, placement maths,
//! drain versus snapshot, mesh validity for every shape, and cache behaviour past its size limit.

use std::collections::HashSet;
use std::sync::Arc;

use regia_core::math::{Color, Mat4, Quat, Vec2, Vec3};
use regia_quick::{Draw3d, Item3d, Quick3dCache, Shape3d};

fn single(draw: &Draw3d) -> Item3d {
    let (items, lines) = draw.drain();
    assert!(lines.is_empty());
    assert_eq!(items.len(), 1);
    items.into_iter().next().expect("one item")
}

#[test]
fn sphere_tessellation_is_clamped_to_a_sane_range() {
    let draw = Draw3d::default();
    draw.sphere_ex(Vec3::ZERO, 1.0, 0, 0);
    assert_eq!(single(&draw).shape, Shape3d::Sphere { rings: 1, slices: 3 });
    draw.sphere_ex(Vec3::ZERO, 1.0, 100_000, 100_000);
    assert_eq!(
        single(&draw).shape,
        Shape3d::Sphere {
            rings: 256,
            slices: 256
        }
    );
    draw.sphere(Vec3::ZERO, 1.0);
    assert_eq!(
        single(&draw).shape,
        Shape3d::Sphere {
            rings: 16,
            slices: 16
        }
    );
}

#[test]
fn cylinders_clamp_sides_and_negative_radii() {
    let draw = Draw3d::default();
    draw.cylinder_ex(Vec3::ZERO, 1.0, 1.0, 2.0, 0);
    let item = single(&draw);
    assert_eq!(
        item.shape,
        Shape3d::Cylinder {
            top: 1.0,
            bottom: 1.0,
            sides: 3
        }
    );
    draw.cylinder_ex(Vec3::ZERO, 1.0, 1.0, 2.0, 5000);
    assert!(matches!(
        single(&draw).shape,
        Shape3d::Cylinder { sides: 256, .. }
    ));

    // A negative radius counts as zero, so this is a cone with a radius-1 base.
    draw.cylinder(Vec3::ZERO, -3.0, 1.0, 1.0);
    assert_eq!(
        single(&draw).shape,
        Shape3d::Cylinder {
            top: 0.0,
            bottom: 1.0,
            sides: 16
        }
    );
}

#[test]
fn nearly_equal_cylinders_share_a_shape_and_scale_carries_the_size() {
    let draw = Draw3d::default();
    draw.cylinder(Vec3::ZERO, 0.5, 1.0, 3.0);
    let a = single(&draw);
    draw.cylinder(Vec3::ZERO, 0.5004, 1.0, 3.0);
    let b = single(&draw);
    assert_eq!(a.shape, b.shape, "ratios are quantized");
    // The larger radius and the height are the model scale; the shape is unit sized.
    let (scale, _, _) = a.model.to_scale_rotation_translation();
    assert!((scale - Vec3::new(1.0, 3.0, 1.0)).length() < 1e-5, "{scale}");
}

#[test]
fn cylinders_with_two_zero_radii_stay_finite() {
    let draw = Draw3d::default();
    draw.cylinder(Vec3::ZERO, 0.0, 0.0, 1.0);
    let item = single(&draw);
    assert!(
        item.model.to_cols_array().iter().all(|v| v.is_finite()),
        "{:?}",
        item.model
    );
    let meshes = Quick3dCache::default().extract(&[item]);
    assert!(
        meshes[0]
            .mesh
            .vertices
            .iter()
            .all(|v| v.position.iter().all(|c| c.is_finite())),
        "no NaN vertices for a degenerate cylinder"
    );
}

#[test]
fn material_parameters_are_clamped() {
    let draw = Draw3d::default();
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .roughness(-1.0)
        .metallic(7.0)
        .emissive(-2.0);
    let item = single(&draw);
    assert_eq!((item.roughness, item.metallic, item.emissive), (0.0, 1.0, 0.0));
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .roughness(7.0)
        .metallic(-7.0)
        .emissive(3.0);
    let item = single(&draw);
    assert_eq!((item.roughness, item.metallic, item.emissive), (1.0, 0.0, 3.0));
    draw.cube(Vec3::ZERO, Vec3::ONE);
    let item = single(&draw);
    assert_eq!(
        (item.roughness, item.metallic, item.emissive),
        (0.5, 0.0, 0.0),
        "defaults"
    );
}

#[test]
fn placement_rotation_and_transform_compose_in_order() {
    let draw = Draw3d::default();
    let center = Vec3::new(1.0, 2.0, 3.0);
    draw.cube(center, Vec3::new(2.0, 4.0, 6.0))
        .rotation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
    let item = single(&draw);
    let (scale, _, translation) = item.model.to_scale_rotation_translation();
    assert!(
        (translation - center).length() < 1e-5,
        "rotation is about the centre"
    );
    assert!((scale - Vec3::new(2.0, 4.0, 6.0)).length() < 1e-5, "{scale}");
    // A quarter turn about Y maps local +X to world -Z.
    let x_axis = item.model.transform_vector3(Vec3::X).normalize();
    assert!((x_axis - -Vec3::Z).length() < 1e-5, "{x_axis}");

    // `transform` applies after the drawing's own placement.
    draw.cube(Vec3::X, Vec3::ONE)
        .transform(regia_core::math::Mat4::from_translation(Vec3::new(
            0.0, 10.0, 0.0,
        )));
    let moved = single(&draw);
    assert!((moved.model.w_axis.truncate() - Vec3::new(1.0, 10.0, 0.0)).length() < 1e-5);
}

#[test]
fn plane_scales_x_and_z_and_leaves_y_alone() {
    let draw = Draw3d::default();
    draw.plane(Vec3::new(0.0, -1.0, 0.0), Vec2::new(5.0, 7.0));
    let item = single(&draw);
    assert_eq!(item.shape, Shape3d::Plane);
    let (scale, _, translation) = item.model.to_scale_rotation_translation();
    assert!((scale - Vec3::new(5.0, 1.0, 7.0)).length() < 1e-5);
    assert!((translation - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-5);
}

#[test]
fn snapshot_keeps_the_frame_and_drain_empties_it() {
    let draw = Draw3d::default();
    assert!(draw.is_empty());
    draw.cube(Vec3::ZERO, Vec3::ONE).color(Color::RED);
    draw.line(Vec3::ZERO, Vec3::Y, Color::GREEN);
    let (items, lines) = draw.snapshot();
    assert_eq!((items.len(), lines.len()), (1, 1));
    assert!(!draw.is_empty(), "a snapshot does not consume");
    assert_eq!(draw.snapshot().0, items);
    let (items2, lines2) = draw.drain();
    assert_eq!((items2, lines2.len()), (items, 1));
    assert!(draw.is_empty());
    assert!(draw.drain().0.is_empty());
    // A clone is another handle to the same lists.
    let other = draw.clone();
    other.sphere(Vec3::ZERO, 1.0);
    assert!(!draw.is_empty());
}

#[test]
fn wires_and_grids_are_offset_by_their_center() {
    let draw = Draw3d::default();
    let center = Vec3::new(10.0, 20.0, 30.0);
    draw.cube_wires(center, Vec3::splat(2.0), Color::WHITE);
    let (items, lines) = draw.drain();
    assert!(items.is_empty());
    assert_eq!(lines.len(), 12, "the 12 edges of a box");
    for (a, b, color) in &lines {
        assert_eq!(*color, Color::WHITE);
        for p in [a, b] {
            let local = (*p - center).abs();
            assert!(local.max_element() <= 1.0 + 1e-5, "within the box: {p}");
        }
    }
    draw.grid(center, 4, 1.0, Color::RED, Color::GRAY);
    let (_, grid) = draw.drain();
    assert!(
        grid.iter().any(|(_, _, c)| *c == Color::RED),
        "axis lines use the axes color"
    );
    assert!(grid.iter().any(|(_, _, c)| *c == Color::GRAY));
    assert!(
        grid.iter()
            .all(|(a, b, _)| (a.y - center.y).abs() < 1e-5 && (b.y - center.y).abs() < 1e-5)
    );
}

#[test]
fn every_shape_extracts_a_valid_triangle_mesh() {
    let draw = Draw3d::default();
    draw.cube(Vec3::ZERO, Vec3::ONE);
    draw.sphere_ex(Vec3::ZERO, 1.0, 2, 3);
    draw.sphere_ex(Vec3::ZERO, 1.0, 256, 256);
    draw.cylinder_ex(Vec3::ZERO, 1.0, 1.0, 1.0, 3);
    draw.cone(Vec3::ZERO, 1.0, 1.0);
    draw.plane(Vec3::ZERO, Vec2::ONE);
    let (items, _) = draw.drain();
    assert_eq!(items.len(), 6);
    for mesh in Quick3dCache::default().extract(&items) {
        let m = &mesh.mesh;
        assert!(!m.vertices.is_empty() && !m.indices.is_empty());
        assert!(m.indices.len().is_multiple_of(3), "whole triangles");
        let max = m.indices.iter().copied().max().expect("indices");
        assert!(
            (max as usize) < m.vertices.len(),
            "indices stay inside the vertex list"
        );
    }
}

#[test]
fn finer_spheres_have_more_vertices() {
    let draw = Draw3d::default();
    draw.sphere_ex(Vec3::ZERO, 1.0, 4, 4);
    draw.sphere_ex(Vec3::ZERO, 1.0, 32, 32);
    let (items, _) = draw.drain();
    let meshes = Quick3dCache::default().extract(&items);
    assert!(meshes[1].mesh.vertices.len() > meshes[0].mesh.vertices.len());
}

#[test]
fn translucent_and_emissive_items_get_their_own_materials() {
    let draw = Draw3d::default();
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .color(Color::new(1.0, 0.0, 0.0, 1.0));
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .color(Color::new(1.0, 0.0, 0.0, 0.5));
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .color(Color::new(1.0, 0.0, 0.0, 1.0))
        .emissive(2.0);
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .color(Color::new(1.0, 0.0, 0.0, 1.0))
        .roughness(0.9);
    draw.cube(Vec3::ZERO, Vec3::ONE)
        .color(Color::new(1.0, 0.0, 0.0, 1.0));
    let (items, _) = draw.drain();
    let meshes = Quick3dCache::default().extract(&items);
    assert!(
        Arc::ptr_eq(&meshes[0].material, &meshes[4].material),
        "same parameters share"
    );
    for (i, other) in meshes.iter().enumerate().skip(1).take(3) {
        assert!(
            !Arc::ptr_eq(&meshes[0].material, &other.material),
            "item {i} differs from the plain red cube"
        );
    }
    assert_eq!(meshes[1].material.base_color[3], 0.5);
    assert_ne!(
        meshes[1].material.alpha_mode, meshes[0].material.alpha_mode,
        "alpha below 1 blends"
    );
    assert_eq!(meshes[2].material.emissive_intensity, 2.0);
    assert_eq!(meshes[3].material.roughness, 0.9);
}

#[test]
fn extraction_past_the_cache_limit_keeps_every_item_correct() {
    let draw = Draw3d::default();
    let count = 1500u32;
    for i in 0..count {
        // A distinct color per item defeats material sharing and overflows the cache.
        draw.cube(Vec3::X * i as f32, Vec3::ONE)
            .color(Color::new(i as f32 / count as f32, 0.25, 0.5, 1.0));
    }
    let (items, _) = draw.drain();
    let mut cache = Quick3dCache::default();
    let meshes = cache.extract(&items);
    assert_eq!(meshes.len(), count as usize);
    let entities: HashSet<_> = meshes.iter().map(|m| m.entity).collect();
    assert_eq!(
        entities.len(),
        count as usize,
        "every drawing gets its own object identity"
    );
    for (i, mesh) in meshes.iter().enumerate() {
        assert_eq!(mesh.model[3][0], i as f32, "item {i} keeps its own placement");
        assert_eq!(
            mesh.material.base_color[0],
            i as f32 / count as f32,
            "item {i} keeps its color"
        );
    }
    // The cache keeps working after it was cleared mid-extract.
    let again = cache.extract(&items[..10]);
    assert_eq!(again.len(), 10);
    assert!(cache.extract(&[]).is_empty());
}

fn assert_transformed_outward(mesh: &regia_render::CpuMesh, model: Mat4, plane: bool) {
    let positions: Vec<Vec3> = mesh
        .vertices
        .iter()
        .map(|vertex| model.transform_point3(Vec3::from_array(vertex.position)))
        .collect();
    let interior = if plane {
        None
    } else {
        let n = positions.len().max(1) as f32;
        Some(
            positions
                .iter()
                .copied()
                .fold(Vec3::ZERO, |sum, point| sum + point)
                / n,
        )
    };
    let (tris, rest) = mesh.indices.as_chunks::<3>();
    assert!(rest.is_empty(), "indices are a triangle list");
    let mut checked = 0u32;
    for tri in tris {
        let [a, b, c] = tri.map(|index| positions[index as usize]);
        let normal = (b - a).cross(c - a);
        if normal.length_squared() < 1e-12 {
            continue;
        }
        checked += 1;
        if let Some(inside) = interior {
            let centroid = (a + b + c) / 3.0;
            assert!(
                normal.dot(centroid - inside) > 0.0,
                "inward normal {normal:?} at {centroid:?} (inside {inside:?})"
            );
        } else {
            assert!(
                normal.y > 0.0,
                "a mirrored plane must still face +Y, got {normal:?}"
            );
        }
    }
    assert!(checked > 0, "the mesh has a non-degenerate triangle");
}

fn expect_mirror(draw_one: impl Fn(&Draw3d), local: Vec3, world: Vec3, plane: bool) {
    let draw = Draw3d::default();
    draw_one(&draw);
    let (items, _) = draw.drain();
    assert_eq!(items.len(), 1);
    let meshes = Quick3dCache::default().extract(&items);
    assert_eq!(meshes.len(), 1);
    let model = Mat4::from_cols_array_2d(&meshes[0].model);
    let got = model.transform_point3(local);
    assert!(
        (got - world).length() < 1e-4,
        "unit point {local:?} mapped to {got:?}, expected {world:?}"
    );
    assert_transformed_outward(&meshes[0].mesh, model, plane);
}

#[test]
fn negative_extents_mirror_and_keep_an_outward_winding() {
    // One negative axis flips the unit mesh. Two negatives do not. A negative sphere radius
    // is three negatives. A negative cylinder height is placed below the base with a positive
    // Y scale, so the unit mesh's origin (its base) maps to `base + (0, height, 0)`.
    expect_mirror(
        |draw| {
            draw.cube(Vec3::ZERO, Vec3::new(-1.0, 1.0, 1.0));
        },
        Vec3::new(0.5, 0.0, 0.0),
        Vec3::new(-0.5, 0.0, 0.0),
        false,
    );
    expect_mirror(
        |draw| {
            draw.cube(Vec3::ZERO, Vec3::new(-1.0, -1.0, 1.0));
        },
        Vec3::new(0.5, 0.5, 0.0),
        Vec3::new(-0.5, -0.5, 0.0),
        false,
    );
    expect_mirror(
        |draw| {
            draw.sphere(Vec3::ZERO, -1.0);
        },
        Vec3::Y,
        -Vec3::Y,
        false,
    );
    expect_mirror(
        |draw| {
            draw.cylinder(Vec3::ZERO, 1.0, 1.0, -2.0);
        },
        Vec3::ZERO,
        Vec3::new(0.0, -2.0, 0.0),
        false,
    );
    expect_mirror(
        |draw| {
            draw.cylinder(Vec3::ZERO, 1.0, 1.0, -2.0);
        },
        Vec3::Y,
        Vec3::ZERO,
        false,
    );
    expect_mirror(
        |draw| {
            draw.plane(Vec3::ZERO, Vec2::new(-2.0, 1.0));
        },
        Vec3::X,
        Vec3::new(-2.0, 0.0, 0.0),
        true,
    );
    expect_mirror(
        |draw| {
            draw.plane(Vec3::ZERO, Vec2::new(-2.0, -1.0));
        },
        Vec3::new(1.0, 0.0, 1.0),
        Vec3::new(-2.0, 0.0, -1.0),
        true,
    );

    let draw = Draw3d::default();
    draw.cube(Vec3::ZERO, Vec3::ONE);
    draw.cube(Vec3::ZERO, Vec3::new(-1.0, 1.0, 1.0));
    draw.cube(Vec3::X, Vec3::new(-2.0, 1.0, 1.0));
    let (items, _) = draw.drain();
    let meshes = Quick3dCache::default().extract(&items);
    assert!(
        !Arc::ptr_eq(&meshes[0].mesh, &meshes[1].mesh),
        "a mirrored cube does not share the outward index buffer"
    );
    assert!(
        Arc::ptr_eq(&meshes[1].mesh, &meshes[2].mesh),
        "mirrored cubes of one shape share the reversed mesh"
    );
}

#[test]
fn entity_identities_are_stable_per_position_across_frames() {
    let draw = Draw3d::default();
    for i in 0..3 {
        draw.sphere(Vec3::X * i as f32, 1.0);
    }
    let (items, _) = draw.drain();
    let mut cache = Quick3dCache::default();
    let first: Vec<_> = cache.extract(&items).iter().map(|m| m.entity).collect();
    let second: Vec<_> = cache.extract(&items).iter().map(|m| m.entity).collect();
    assert_eq!(
        first, second,
        "the GPU object cache sees the same ids frame to frame"
    );
}

/// Finding 1 (3D): many segments share one lock.
#[test]
fn perf_lines_lock_once() {
    let draw = Draw3d::default();
    let before = draw.lock_count();
    draw.lines([
        (Vec3::ZERO, Vec3::X, Color::RED),
        (Vec3::Y, Vec3::Z, Color::GREEN),
    ]);
    assert_eq!(draw.lock_count() - before, 1, "two segments, one lock");
    let (_, lines) = draw.snapshot();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].2, Color::RED);
    assert_eq!(lines[1].2, Color::GREEN);
}

/// Finding 6: drain keeps list capacity, and a frozen frame with an empty draw reuses the last
/// submission instead of cloning an empty snapshot.
#[test]
fn perf_3d_lists_keep_capacity() {
    let draw = Draw3d::default();
    let mut cache = Quick3dCache::default();
    for i in 0..4 {
        draw.cube(Vec3::new(i as f32, 0.0, 0.0), Vec3::ONE);
        draw.line(Vec3::ZERO, Vec3::new(0.0, i as f32, 0.0), Color::WHITE);
    }
    assert_eq!(cache.take_frame(&draw, false), 4);
    assert!(
        draw.items_capacity() >= 4,
        "item capacity after the first frame: {}",
        draw.items_capacity()
    );
    assert!(
        draw.lines_capacity() >= 4,
        "line capacity after the first frame: {}",
        draw.lines_capacity()
    );
    let ptr = cache.retained_items_ptr();
    assert_ne!(ptr, 0);

    for i in 0..4 {
        draw.cube(Vec3::new(i as f32, 1.0, 0.0), Vec3::ONE);
        draw.line(Vec3::X, Vec3::new(0.0, i as f32, 1.0), Color::RED);
    }
    assert_eq!(cache.take_frame(&draw, false), 4);
    assert!(draw.items_capacity() >= 4);
    assert_eq!(cache.retained_items_ptr(), ptr, "the item allocation moved");

    let mut out = Vec::new();
    cache.extend_extracted(&mut out);
    assert_eq!(out.len(), 4);
    let allocs = cache.extract_allocs();
    cache.extend_extracted(&mut out);
    assert_eq!(
        cache.extract_allocs(),
        allocs,
        "the second extract allocated another instance vector"
    );

    // The view did not run: the draw is empty, and the frozen frame keeps the last four cubes.
    assert_eq!(cache.take_frame(&draw, true), 4);
    assert_eq!(cache.items().len(), 4);
    assert_eq!(cache.lines().len(), 4);
    assert_eq!(cache.retained_items_ptr(), ptr);
}
