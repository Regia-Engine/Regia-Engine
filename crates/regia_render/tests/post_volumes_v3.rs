// SPDX-License-Identifier: MPL-2.0
//! Post-volume format v3 (`docs/specs/30-post-volumes-and-physical-camera.md` § 10, ADR-2323):
//! the convex-hull shape and its exact falloff, the sphere's falloff, the gizmo outlines, the
//! per-volume LUT blend (off by default), the v2 → v3 migration and the order in which the
//! project, the level look, the camera and the volumes override each other. CPU only.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::id::TypePathHash;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_render::post_volume_luts::{
    LutContribution, MAX_CACHED_BLENDS, PostVolumeLuts, blend_luts, load_post_volume_luts,
};
use regia_render::post_volume_shapes::{
    MAX_HULL_PLANES, SPHERE_OUTLINE_SEGMENTS, box_hull_planes, hull_planes_valid, hull_signed_distance,
    hull_signed_distance_within, post_volume_outline,
};
use regia_render::post_volumes::{
    POST_VOLUME_FORMAT_VERSION, PostOverrides, PostVolume, PostVolumeShape, post_volume_migration_sources,
    post_volume_schema_fingerprint,
};
use regia_render::{
    Camera, CameraExposureMode, ColorLut, ExtractedFrame, LensEffects, LevelColorLut, PhysicalCamera,
    PostSettings, RenderProfilePreset, RenderSettings, extract_frame,
};
use regia_scene::{GlobalTransform, Transform};

fn at(translation: [f32; 3]) -> GlobalTransform {
    GlobalTransform(Transform::from_translation(Vec3::from_array(translation)).compute_affine())
}

fn placed(scale: Vec3, rotation: Quat, translation: Vec3) -> GlobalTransform {
    GlobalTransform(Affine3A::from_scale_rotation_translation(
        scale,
        rotation,
        translation,
    ))
}

fn hull(planes: Vec<[f32; 4]>, blend_distance: f32) -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::ConvexHull { planes },
        blend_distance,
        ..PostVolume::default()
    }
}

/// The regular-ish tetrahedron with vertices (0,0,0), (1,0,0), (0,1,0), (0,0,1).
fn corner_tetrahedron() -> Vec<[f32; 4]> {
    let s = 1.0 / 3.0f32.sqrt();
    vec![
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
        [0.0, 0.0, -1.0, 0.0],
        [s, s, s, s],
    ]
}

/// A deterministic spread of points around a shape, including face, edge and corner regions.
fn probe_points(extent: f32) -> Vec<[f32; 3]> {
    let mut out = Vec::new();
    let steps = [-1.6f32, -1.05, -0.7, -0.2, 0.0, 0.35, 0.9, 1.0, 1.3, 2.1];
    for x in steps {
        for y in steps {
            for z in steps {
                out.push([x * extent, y * extent * 0.8, z * extent * 1.2]);
            }
        }
    }
    out
}

#[test]
fn a_box_hull_matches_the_box_shape_under_rotation_and_scale() {
    let half = [1.5, 0.75, 2.0];
    let boxed = PostVolume {
        shape: PostVolumeShape::Box { half_extents: half },
        ..PostVolume::default()
    };
    let as_hull = hull(box_hull_planes(half), 0.0);
    for transform in [
        GlobalTransform::IDENTITY,
        placed(Vec3::ONE, Quat::from_rotation_y(0.7), Vec3::new(3.0, -1.0, 2.0)),
        placed(
            Vec3::new(2.0, 0.5, 1.25),
            Quat::from_euler(regia_core::math::EulerRot::XYZ, 0.3, -0.9, 0.2),
            Vec3::new(-4.0, 2.0, 7.0),
        ),
    ] {
        for local in probe_points(2.5) {
            let p = transform.0.transform_point3(Vec3::from_array(local)).to_array();
            let expected = boxed.signed_distance(&transform, p);
            let got = as_hull.signed_distance(&transform, p);
            assert!(
                (expected - got).abs() <= 1.0e-4 * (1.0 + expected.abs()),
                "at {p:?}: box {expected}, hull {got}"
            );
        }
    }
}

#[test]
fn the_hull_distance_is_exact_at_faces_edges_and_vertices() {
    let planes = corner_tetrahedron();
    let world = GlobalTransform::IDENTITY;
    // Face region: straight out of the slanted face from its centroid.
    let c = 1.0 / 3.0f32;
    let n = 1.0 / 3.0f32.sqrt();
    let p = [c + 0.5 * n, c + 0.5 * n, c + 0.5 * n];
    assert!((hull_signed_distance(&planes, &world, p) - 0.5).abs() < 1.0e-5);
    // Vertex region: beyond the apex (0,0,1), the nearest point is the apex itself.
    let p = [-0.3, -0.4, 1.5];
    let expected = (0.09f32 + 0.16 + 0.25).sqrt();
    assert!((hull_signed_distance(&planes, &world, p) - expected).abs() < 1.0e-5);
    // Edge region: beyond the edge x = y = 0 (the z axis segment), at z = 0.5.
    let p = [-0.3, -0.4, 0.5];
    assert!((hull_signed_distance(&planes, &world, p) - 0.5).abs() < 1.0e-5);
    // Below the origin vertex.
    let p = [-1.0, -2.0, -2.0];
    assert!((hull_signed_distance(&planes, &world, p) - 3.0).abs() < 1.0e-5);
    // Inside: the distance to the nearest face plane, negative.
    let p = [0.1, 0.2, 0.3];
    assert!((hull_signed_distance(&planes, &world, p) + 0.1).abs() < 1.0e-6);
    // The plain plane bound is not the distance at a vertex (it would be too small).
    let bound = 1.5f32 - 1.0;
    assert!(hull_signed_distance(&planes, &world, [-0.3, -0.4, 1.5]) > bound + 0.1);
}

#[test]
fn the_hull_band_is_a_smoothstep_of_the_exact_distance() {
    let volume = hull(corner_tetrahedron(), 2.0);
    let world = GlobalTransform::IDENTITY;
    assert_eq!(volume.weight_at(&world, [0.1, 0.1, 0.1]), 1.0);
    assert_eq!(volume.weight_at(&world, [0.0, 0.0, 0.0]), 1.0, "on a vertex");
    // 1 m beyond the apex along +z: mid-band, Hermite(0.5) = 0.5.
    let mid = volume.weight_at(&world, [0.0, 0.0, 2.0]);
    assert!((mid - 0.5).abs() < 1.0e-5, "{mid}");
    assert_eq!(volume.weight_at(&world, [0.0, 0.0, 3.0]), 0.0, "band edge");
    assert_eq!(volume.weight_at(&world, [0.0, 0.0, 30.0]), 0.0);
    // Monotonic and continuous on a walk away from a corner in 1 cm steps.
    let mut last = 1.0f32;
    for i in 0..400 {
        let t = i as f32 * 0.01;
        let w = volume.weight_at(&world, [-t * 0.6, -t * 0.8, 1.0 + t * 0.3]);
        assert!(w <= last + 1.0e-6, "rises at step {i}");
        assert!((last - w).abs() < 0.02, "jumps at step {i}: {last} -> {w}");
        last = w;
    }
    assert_eq!(last, 0.0);
}

#[test]
fn the_reach_only_skips_points_beyond_it() {
    let planes = corner_tetrahedron();
    let world = GlobalTransform::IDENTITY;
    let near = [-0.3, -0.4, 1.5];
    assert_eq!(
        hull_signed_distance_within(&planes, &world, near, 1.0),
        hull_signed_distance(&planes, &world, near)
    );
    let far = [-30.0, -40.0, 150.0];
    let cheap = hull_signed_distance_within(&planes, &world, far, 1.0);
    assert!(cheap > 1.0 && cheap <= hull_signed_distance(&planes, &world, far));
}

#[test]
fn invalid_and_empty_hulls_never_contribute() {
    let world = GlobalTransform::IDENTITY;
    let seventeen: Vec<[f32; 4]> = (0..=MAX_HULL_PLANES).map(|i| [1.0, i as f32, 0.0, 1.0]).collect();
    for planes in [
        Vec::new(),
        seventeen,
        vec![[0.0, 0.0, 0.0, 1.0]],
        vec![[f32::NAN, 1.0, 0.0, 1.0]],
        vec![[1.0, 0.0, 0.0, f32::INFINITY]],
    ] {
        assert!(!hull_planes_valid(&planes), "{planes:?}");
        let volume = hull(planes, 5.0);
        assert!(!volume.shape.is_valid());
        assert_eq!(volume.weight_at(&world, [0.0; 3]), 0.0);
        assert!(post_volume_outline(&volume, &world).is_none());
    }
    // x ≤ −1 and x ≥ 1 do not meet: nowhere is inside, nothing is near.
    let empty = hull(vec![[1.0, 0.0, 0.0, -1.0], [-1.0, 0.0, 0.0, -1.0]], 5.0);
    assert!(empty.shape.is_valid());
    for x in [-3.0, -1.0, 0.0, 1.0, 3.0] {
        assert_eq!(empty.weight_at(&world, [x, 0.0, 0.0]), 0.0, "x = {x}");
    }
    // A degenerate transform and a non-finite eye count as outside.
    let flat = GlobalTransform(Affine3A::from_scale(Vec3::new(1.0, 0.0, 1.0)));
    let cube = hull(box_hull_planes([1.0; 3]), 1.0);
    assert_eq!(cube.weight_at(&flat, [0.0; 3]), 0.0);
    assert_eq!(cube.weight_at(&world, [f32::NAN, 0.0, 0.0]), 0.0);
    // Exactly 16 planes is allowed.
    let sixteen: Vec<[f32; 4]> = (0..MAX_HULL_PLANES)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / MAX_HULL_PLANES as f32;
            [a.cos(), 0.0, a.sin(), 2.0]
        })
        .collect();
    let prism = hull(sixteen, 0.0);
    assert!(prism.shape.is_valid());
    assert_eq!(
        prism.weight_at(&world, [0.5, 100.0, -0.5]),
        1.0,
        "an infinite prism"
    );
}

#[test]
fn an_unbounded_hull_is_a_half_space_with_a_band() {
    let floor = hull(vec![[0.0, 1.0, 0.0, 0.0]], 1.0);
    let world = at([0.0, 2.0, 0.0]);
    assert_eq!(floor.weight_at(&world, [100.0, 1.0, -50.0]), 1.0);
    assert!((floor.weight_at(&world, [0.0, 2.5, 0.0]) - 0.5).abs() < 1.0e-6);
    assert_eq!(floor.weight_at(&world, [0.0, 3.0, 0.0]), 0.0);
    let outline = post_volume_outline(&floor, &world).expect("a valid hull outlines");
    assert!(!outline.bounded, "a half-space has no closed outline");
    assert!(outline.shape.is_empty());
    // A wedge (two planes) is unbounded too, its edge line has no vertices.
    let wedge = hull(vec![[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]], 0.0);
    assert!(post_volume_outline(&wedge, &world).is_some_and(|o| !o.bounded));
}

#[test]
fn the_sphere_band_is_exact_in_world_meters() {
    let sphere = PostVolume {
        shape: PostVolumeShape::Sphere { radius: 2.0 },
        blend_distance: 4.0,
        ..PostVolume::default()
    };
    let world = placed(
        Vec3::splat(1.5),
        Quat::from_rotation_x(1.1),
        Vec3::new(1.0, 2.0, 3.0),
    );
    let center = Vec3::new(1.0, 2.0, 3.0);
    let dir = Vec3::new(0.3, -0.5, 0.8).normalize();
    // World radius 3; mid-band at 5 m from the centre in any direction.
    for (d, expected) in [(1.0f32, 1.0f32), (3.0, 1.0), (5.0, 0.5), (7.0, 0.0), (9.0, 0.0)] {
        let p = (center + dir * d).to_array();
        assert!((sphere.signed_distance(&world, p) - (d - 3.0)).abs() < 1.0e-4);
        assert!((sphere.weight_at(&world, p) - expected).abs() < 1.0e-5, "d = {d}");
    }
}

#[test]
fn outlines_trace_each_shape_and_its_band() {
    let world = placed(Vec3::new(2.0, 1.0, 1.0), Quat::IDENTITY, Vec3::ZERO);
    // Box: twelve edges, the band pushed out by the blend distance in world meters.
    let boxed = PostVolume {
        shape: PostVolumeShape::Box {
            half_extents: [1.0, 2.0, 3.0],
        },
        blend_distance: 0.5,
        ..PostVolume::default()
    };
    let outline = post_volume_outline(&boxed, &world).expect("box outline");
    assert_eq!(outline.shape.len(), 12);
    assert_eq!(outline.band.len(), 12);
    assert!(outline.bounded);
    let max_x = |segments: &[[[f32; 3]; 2]]| segments.iter().flatten().map(|p| p[0]).fold(f32::MIN, f32::max);
    assert!((max_x(&outline.shape) - 2.0).abs() < 1.0e-5);
    assert!((max_x(&outline.band) - 2.5).abs() < 1.0e-5);
    // The same box as a hull: the same twelve edges, the same band.
    let as_hull = PostVolume {
        shape: PostVolumeShape::ConvexHull {
            planes: box_hull_planes([1.0, 2.0, 3.0]),
        },
        ..boxed.clone()
    };
    let hull_outline = post_volume_outline(&as_hull, &world).expect("hull outline");
    assert_eq!(hull_outline.shape.len(), 12);
    assert!(hull_outline.bounded);
    assert!((max_x(&hull_outline.shape) - 2.0).abs() < 1.0e-5);
    assert!((max_x(&hull_outline.band) - 2.5).abs() < 1.0e-5);
    // A tetrahedron has six edges; no band without a blend distance.
    let tet = hull(corner_tetrahedron(), 0.0);
    let tet_outline = post_volume_outline(&tet, &GlobalTransform::IDENTITY).expect("tetrahedron");
    assert_eq!(tet_outline.shape.len(), 6);
    assert!(tet_outline.band.is_empty());
    // Sphere: three great circles each for the shape and the band.
    let sphere = PostVolume {
        shape: PostVolumeShape::Sphere { radius: 1.0 },
        blend_distance: 1.0,
        ..PostVolume::default()
    };
    let sphere_outline = post_volume_outline(&sphere, &GlobalTransform::IDENTITY).expect("sphere");
    assert_eq!(sphere_outline.shape.len(), 3 * SPHERE_OUTLINE_SEGMENTS);
    assert_eq!(sphere_outline.band.len(), 3 * SPHERE_OUTLINE_SEGMENTS);
    assert!((max_x(&sphere_outline.band) - 2.0).abs() < 1.0e-5);
    // Unbound covers the world: nothing to outline.
    let unbound = PostVolume {
        shape: PostVolumeShape::Unbound,
        ..PostVolume::default()
    };
    assert!(post_volume_outline(&unbound, &GlobalTransform::IDENTITY).is_none());
}

// -- LUT blending ---------------------------------------------------------------------------

fn warm(size: u32) -> Arc<ColorLut> {
    Arc::new(ColorLut::from_fn(size, |c| [c[0].powf(0.8), c[1], c[2] * 0.7]))
}

fn cold(size: u32) -> Arc<ColorLut> {
    Arc::new(ColorLut::from_fn(size, |c| [c[0] * 0.6, c[1] * 0.9, c[2].sqrt()]))
}

fn lut_at(weight: f32, priority: i32, key: u64, lut: &Arc<ColorLut>) -> LutContribution {
    LutContribution {
        weight,
        priority,
        key,
        lut: lut.clone(),
    }
}

fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
}

const PROBES: [[f32; 3]; 5] = [
    [0.0, 0.0, 0.0],
    [0.2, 0.5, 0.9],
    [0.77, 0.31, 0.05],
    [1.0, 1.0, 1.0],
    [0.5, 0.5, 0.5],
];

#[test]
fn no_lut_volume_keeps_the_base_and_one_full_volume_hands_its_table_over() {
    let base = cold(17);
    assert!(blend_luts(None, &mut []).is_none());
    let kept = blend_luts(Some(&base), &mut []).expect("the base");
    assert!(Arc::ptr_eq(&kept, &base));
    let lut = warm(17);
    let full = blend_luts(Some(&base), &mut [lut_at(1.0, 0, 1, &lut)]).expect("the volume's");
    assert!(
        Arc::ptr_eq(&full, &lut),
        "no copy for a single full-weight volume"
    );
    // Zero, negative and NaN weights count for nothing.
    let none = blend_luts(
        Some(&base),
        &mut [
            lut_at(0.0, 0, 1, &lut),
            lut_at(f32::NAN, 1, 2, &lut),
            lut_at(-1.0, 2, 3, &lut),
        ],
    )
    .expect("the base");
    assert!(Arc::ptr_eq(&none, &base));
}

#[test]
fn a_partial_weight_blends_the_looked_up_colours() {
    let base = cold(9);
    let lut = warm(9);
    let half = blend_luts(Some(&base), &mut [lut_at(0.5, 0, 1, &lut)]).expect("blend");
    assert_eq!(half.size(), 9);
    for c in PROBES {
        let a = base.sample(c);
        let b = lut.sample(c);
        let expected = [0.5 * (a[0] + b[0]), 0.5 * (a[1] + b[1]), 0.5 * (a[2] + b[2])];
        assert!(close(half.sample(c), expected, 1.0e-5), "{c:?}");
    }
    // Over the identity (no level LUT) the same rule holds.
    let over_identity = blend_luts(None, &mut [lut_at(0.25, 0, 1, &lut)]).expect("blend");
    for c in PROBES {
        let b = lut.sample(c);
        let expected = [
            0.75 * c[0] + 0.25 * b[0],
            0.75 * c[1] + 0.25 * b[1],
            0.75 * c[2] + 0.25 * b[2],
        ];
        assert!(close(over_identity.sample(c), expected, 1.0e-5), "{c:?}");
    }
}

#[test]
fn lut_tiers_follow_priority_and_equal_priorities_average_in_any_order() {
    let a = warm(5);
    let b = cold(5);
    // A full higher tier replaces the lower one.
    let high = blend_luts(None, &mut [lut_at(1.0, 0, 1, &a), lut_at(1.0, 5, 2, &b)]).expect("blend");
    assert!(Arc::ptr_eq(&high, &b));
    let high_reversed = blend_luts(None, &mut [lut_at(1.0, 5, 2, &b), lut_at(1.0, 0, 1, &a)]).expect("blend");
    assert!(Arc::ptr_eq(&high_reversed, &b));
    // A half-weight higher tier lerps from the lower tier's table.
    let lerped = blend_luts(None, &mut [lut_at(1.0, 0, 1, &a), lut_at(0.5, 5, 2, &b)]).expect("blend");
    for c in PROBES {
        let (x, y) = (a.sample(c), b.sample(c));
        let expected = [0.5 * (x[0] + y[0]), 0.5 * (x[1] + y[1]), 0.5 * (x[2] + y[2])];
        assert!(close(lerped.sample(c), expected, 1.0e-5), "{c:?}");
    }
    // Equal priorities at full weight average, bit for bit in either order.
    let one = blend_luts(None, &mut [lut_at(1.0, 2, 1, &a), lut_at(1.0, 2, 2, &b)]).expect("blend");
    let two = blend_luts(None, &mut [lut_at(1.0, 2, 2, &b), lut_at(1.0, 2, 1, &a)]).expect("blend");
    assert_eq!(one.data(), two.data());
    assert_eq!(one.key(), two.key());
    assert!(close(
        one.sample([0.2, 0.5, 0.9]),
        lerped.sample([0.2, 0.5, 0.9]),
        1.0e-5
    ));
}

#[test]
fn tables_of_different_sizes_blend_on_the_larger_grid() {
    let small = warm(3);
    let large = cold(33);
    let blend = blend_luts(Some(&large), &mut [lut_at(0.5, 0, 1, &small)]).expect("blend");
    assert_eq!(blend.size(), 33);
    // At the large grid's points the result is the blend of the two lookups.
    let f = 32.0f32;
    for [r, g, b] in [[0u32, 0, 0], [16, 8, 31], [32, 32, 32], [5, 27, 13]] {
        let c = [r as f32 / f, g as f32 / f, b as f32 / f];
        let (x, y) = (large.sample(c), small.sample(c));
        let expected = [0.5 * (x[0] + y[0]), 0.5 * (x[1] + y[1]), 0.5 * (x[2] + y[2])];
        assert!(close(blend.sample(c), expected, 1.0e-5), "{c:?}");
    }
}

#[test]
fn the_lut_blend_is_continuous_in_the_weight() {
    let base = cold(9);
    let lut = warm(9);
    let mut last = base.sample([0.6, 0.3, 0.8]);
    for i in 1..=100 {
        let w = i as f32 / 100.0;
        let now = blend_luts(Some(&base), &mut [lut_at(w, 0, 1, &lut)])
            .expect("blend")
            .sample([0.6, 0.3, 0.8]);
        assert!(close(now, last, 0.01), "jump at {w}: {last:?} -> {now:?}");
        last = now;
    }
    assert!(close(last, lut.sample([0.6, 0.3, 0.8]), 1.0e-6));
}

fn lut_volume(path: &str, weight: f32, priority: i32) -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::Unbound,
        weight,
        priority,
        lut: Some(path.into()),
        ..PostVolume::default()
    }
}

#[test]
fn the_resource_is_off_by_default_and_caches_blends() {
    let mut luts = PostVolumeLuts::default();
    assert!(!luts.enabled, "off by default");
    luts.insert("warm.cube", warm(9));
    let volume = lut_volume("warm.cube", 0.5, 0);
    let world = GlobalTransform::IDENTITY;
    let entity = Entity::from_raw_u32(7).expect("entity index");
    let mut post = PostSettings::default();
    luts.apply([(entity, &volume, &world)], [0.0; 3], &mut post);
    assert_eq!(post, PostSettings::default(), "nothing while off");
    assert_eq!(luts.stats().blends, 0);

    luts.enabled = true;
    luts.apply([(entity, &volume, &world)], [0.0; 3], &mut post);
    let first = post.lut.clone().expect("blended");
    assert_eq!(luts.stats().blends, 1);
    let mut again = PostSettings::default();
    luts.apply([(entity, &volume, &world)], [0.0; 3], &mut again);
    assert!(Arc::ptr_eq(&first, again.lut.as_ref().expect("cached")));
    assert_eq!(luts.stats().cache_hits, 1);
    // A volume naming an unloaded path, or a newer-format volume, adds nothing.
    let unknown = lut_volume("missing.cube", 1.0, 3);
    let newer = PostVolume {
        format_version: POST_VOLUME_FORMAT_VERSION + 1,
        ..lut_volume("warm.cube", 1.0, 4)
    };
    let mut post = PostSettings::default();
    luts.apply(
        [(entity, &unknown, &world), (entity, &newer, &world)],
        [0.0; 3],
        &mut post,
    );
    assert_eq!(post.lut, None);
    // The cache holds at most MAX_CACHED_BLENDS tables.
    for i in 0..(MAX_CACHED_BLENDS as u32 + 3) {
        let v = lut_volume("warm.cube", 0.1 + 0.05 * i as f32, 0);
        let mut post = PostSettings::default();
        luts.apply([(entity, &v, &world)], [0.0; 3], &mut post);
    }
    assert_eq!(luts.stats().blends, 1 + MAX_CACHED_BLENDS as u64 + 3);
}

#[test]
fn luts_load_from_files_once_and_failures_are_not_retried() {
    let dir = regia_test_utils::unique_temp_dir("post-volume-luts");
    let cube = dir.join("swap.cube");
    let mut text = String::from("LUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                text.push_str(&format!("{b} {g} {r}\n"));
            }
        }
    }
    let tmp = dir.join("swap.cube.tmp");
    std::fs::write(&tmp, text).expect("write the LUT");
    std::fs::rename(&tmp, &cube).expect("publish the LUT");
    let good = cube.display().to_string();
    let bad = dir.join("missing.cube").display().to_string();

    let mut app = App::new();
    app.init_resource::<PostVolumeLuts>();
    app.world_mut().spawn((lut_volume(&good, 1.0, 0), at([0.0; 3])));
    app.world_mut().spawn((lut_volume(&bad, 1.0, 1), at([0.0; 3])));
    app.add_systems(Render, load_post_volume_luts);
    app.update();
    assert!(
        app.world().resource::<PostVolumeLuts>().paths().is_empty(),
        "nothing loads while off"
    );
    app.world_mut().resource_mut::<PostVolumeLuts>().enabled = true;
    app.update();
    app.update();
    let luts = app.world().resource::<PostVolumeLuts>();
    assert_eq!(luts.table(&good).map(|l| l.size()), Some(2));
    assert!(luts.failed(&bad));
    assert_eq!(luts.stats().loads, 1);
    assert_eq!(
        luts.stats().failures,
        1,
        "a failed path is not retried every frame"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Same resolution as the level LUT: a relative `looks/hall.cube` served only by the cooked
/// cache (no loose file) loads for a post volume, and `looks\hall.cube` is the same path.
#[test]
fn volume_luts_resolve_through_the_cooked_cache_and_accept_backslashes() {
    let root = regia_test_utils::unique_temp_dir("post-volume-luts-cooked");
    let cache = regia_asset::cooked_cache_dir(&root, "linux");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let id = regia_asset::AssetId::new_random();
    let mut text = String::from("LUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                text.push_str(&format!("{b} {g} {r}\n"));
            }
        }
    }
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: TypePathHash::of("regia_render::lut::ColorLut"),
        asset_id: id,
        importer_version: regia_asset::pack_importer_version(1, 0, 0),
        dependencies: vec![],
        payload_len: text.len() as u64,
    };
    let file = regia_asset::encode_cooked_file(&header, text.as_bytes()).expect("encode");
    std::fs::write(cache.join(regia_asset::cooked_file_name(id)), &file).expect("cooked");
    let manifest = regia_asset::AssetManifest {
        entries: vec![regia_asset::ManifestEntry {
            // The real cook emits the project-relative path (asserted in cook.rs), so the
            // resolver's primary `assets/<path>` key is the one a cooked project produces.
            path: "assets/looks/hall.cube".to_string(),
            id,
            type_path: "regia_render::lut::ColorLut".to_string(),
        }],
    };
    manifest.write_to_cache(&cache).expect("manifest");
    let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
        project_root: root.clone(),
        platform: "linux".to_string(),
        content_dir: None,
    })
    .expect("server");

    let mut app = App::new();
    app.init_resource::<PostVolumeLuts>();
    app.insert_resource(server);
    app.world_mut()
        .spawn((lut_volume("looks/hall.cube", 1.0, 0), at([0.0; 3])));
    app.world_mut()
        .spawn((lut_volume("looks\\hall.cube", 1.0, 0), at([0.0; 3])));
    app.add_systems(Render, load_post_volume_luts);
    app.world_mut().resource_mut::<PostVolumeLuts>().enabled = true;
    app.update();
    let luts = app.world().resource::<PostVolumeLuts>();
    let forward = luts.table("looks/hall.cube").expect("the cooked LUT loads");
    let backslash = luts
        .table("looks\\hall.cube")
        .expect("the backslash path resolves");
    assert_eq!(forward.key(), backslash.key());
    assert_eq!(luts.stats().loads, 2);
    assert_eq!(luts.stats().failures, 0);
    let _ = std::fs::remove_dir_all(&root);
}

struct Scene {
    app: App,
}

impl Scene {
    fn new(settings: RenderSettings) -> Self {
        let mut app = App::new();
        app.init_resource::<ExtractedFrame>();
        app.insert_resource(settings);
        app.add_systems(Render, extract_frame);
        Self { app }
    }

    fn volume(mut self, volume: PostVolume) -> Self {
        self.app.world_mut().spawn((volume, at([0.0; 3])));
        self
    }

    fn post(mut self, camera: impl Bundle) -> PostSettings {
        self.app.world_mut().spawn((at([0.0, 1.0, 0.0]), camera));
        self.app.update();
        let frame = self.app.world().resource::<ExtractedFrame>();
        assert_eq!(frame.views.len(), 1);
        frame.views[0].post.clone()
    }
}

fn unbound(overrides: PostOverrides) -> PostVolume {
    PostVolume {
        shape: PostVolumeShape::Unbound,
        overrides,
        ..PostVolume::default()
    }
}

#[test]
fn a_camera_exposure_pin_beats_the_volume_band_but_keeps_its_compensation() {
    let volume = unbound(PostOverrides {
        exposure_min_ev100: Some(2.0),
        exposure_max_ev100: Some(4.0),
        exposure_compensation: Some(1.5),
        ..PostOverrides::default()
    });
    let pinned = Scene::new(RenderSettings::default())
        .volume(volume.clone())
        .post(Camera {
            exposure_ev: Some(11.0),
            ..Camera::default()
        });
    assert!(!pinned.auto_exposure, "the pin turns auto exposure off");
    assert_eq!(pinned.exposure_ev, 11.0);
    assert_eq!(
        pinned.exposure_compensation, 1.5,
        "the volume's dial still applies"
    );
    // Without the pin the volume's band is the auto-exposure band.
    let free = Scene::new(RenderSettings::default())
        .volume(volume)
        .post(Camera::default());
    assert_eq!(
        (free.auto_exposure_min_ev100, free.auto_exposure_max_ev100),
        (2.0, 4.0)
    );
}

#[test]
fn a_manual_or_physical_lens_beats_the_pin_and_the_volume() {
    let volume = unbound(PostOverrides {
        exposure_compensation: Some(-0.5),
        ..PostOverrides::default()
    });
    let lens = PhysicalCamera {
        exposure: CameraExposureMode::Manual { ev100: 9.0 },
        ..PhysicalCamera::default()
    };
    let post = Scene::new(RenderSettings::default())
        .volume(volume.clone())
        .post((
            Camera {
                exposure_ev: Some(13.0),
                ..Camera::default()
            },
            lens,
        ));
    assert_eq!(post.exposure_ev, 9.0, "the lens wins over the camera pin");
    assert!(!post.auto_exposure);
    assert_eq!(
        post.exposure_compensation, -0.5,
        "and the volume's compensation stays"
    );
    let physical = PhysicalCamera {
        exposure: CameraExposureMode::Physical,
        ..PhysicalCamera::default()
    };
    let post = Scene::new(RenderSettings::default())
        .volume(volume)
        .post((Camera::default(), physical));
    assert!(post.use_physical_camera);
    assert_eq!(post.exposure_ev, physical.ev100());
    assert_eq!(post.exposure_compensation, -0.5);
}

#[test]
fn volumes_override_the_cameras_lens_effects_and_the_preset_gates_both() {
    let volume = unbound(PostOverrides {
        chromatic_aberration: Some(0.01),
        ..PostOverrides::default()
    });
    let cinematic = RenderSettings::from_profile(RenderProfilePreset::Cinematic);
    // The camera's component replaces the project's; the volume overrides the camera's field.
    let post = Scene::new(cinematic.clone())
        .volume(volume.clone())
        .post((Camera::default(), LensEffects::CINEMATIC));
    assert_eq!(post.lens_effects.chromatic_aberration, 0.01);
    assert_eq!(
        post.lens_effects.dirt_intensity,
        LensEffects::CINEMATIC.dirt_intensity,
        "fields the volume does not set keep the camera's"
    );
    // A preset without the pass turns them off, whatever the camera and volume ask.
    let mobile = RenderSettings::from_profile(RenderProfilePreset::MobileHigh);
    let post = Scene::new(mobile)
        .volume(volume)
        .post((Camera::default(), LensEffects::CINEMATIC));
    assert!(!post.lens_effects.is_active());
}

#[test]
fn the_cameras_motion_blur_and_focus_are_not_volume_settings() {
    let volume = unbound(PostOverrides {
        saturation: Some(0.3),
        ..PostOverrides::default()
    });
    let lens = PhysicalCamera {
        depth_of_field: Some(regia_render::DepthOfFieldMode::Gaussian),
        ..PhysicalCamera::default()
    };
    let cinematic = RenderSettings::from_profile(RenderProfilePreset::Cinematic);
    let with = Scene::new(cinematic.clone())
        .volume(volume)
        .post((Camera::default(), lens));
    let without = Scene::new(cinematic).post((Camera::default(), lens));
    assert!((with.saturation - 0.3).abs() < 1.0e-6, "{}", with.saturation);
    assert!(
        without.depth_of_field.is_some(),
        "the lens's focus is on in this preset"
    );
    assert_eq!(with.depth_of_field, without.depth_of_field);
    assert_eq!(with.motion_blur, without.motion_blur);
}

#[test]
fn a_volume_lut_blends_over_the_level_lut_only_when_turned_on() {
    let level = cold(9);
    let room = warm(9);
    let scene = |enabled: bool, weight: f32| {
        let mut scene = Scene::new(RenderSettings::default()).volume(lut_volume("room.cube", weight, 0));
        scene.app.insert_resource(LevelColorLut {
            path: Some("level.cube".into()),
            lut: Some(level.clone()),
        });
        let mut luts = PostVolumeLuts::default();
        luts.insert("room.cube", room.clone());
        luts.enabled = enabled;
        scene.app.insert_resource(luts);
        scene.post(Camera::default())
    };
    // Off (the default): the level's LUT, exactly the frame without volume LUTs.
    let off = scene(false, 1.0);
    assert!(Arc::ptr_eq(off.lut.as_ref().expect("level LUT"), &level));
    let mut without_volumes = Scene::new(RenderSettings::default());
    without_volumes.app.insert_resource(LevelColorLut {
        path: Some("level.cube".into()),
        lut: Some(level.clone()),
    });
    assert_eq!(off, without_volumes.post(Camera::default()));
    // On at full weight: the room's own table.
    let on = scene(true, 1.0);
    assert!(Arc::ptr_eq(on.lut.as_ref().expect("room LUT"), &room));
    // On at half weight: the blend of the two.
    let half = scene(true, 0.5);
    let blended = half.lut.expect("blend");
    for c in PROBES {
        let (x, y) = (level.sample(c), room.sample(c));
        let expected = [0.5 * (x[0] + y[0]), 0.5 * (x[1] + y[1]), 0.5 * (x[2] + y[2])];
        assert!(close(blended.sample(c), expected, 1.0e-5), "{c:?}");
    }
}

// -- Format v3 --------------------------------------------------------------------------------

#[test]
fn format_v3_migrates_v2_and_names_the_new_shape_and_lut() {
    assert_eq!(POST_VOLUME_FORMAT_VERSION, 3);
    assert_eq!(post_volume_migration_sources(), vec![0, 1, 2]);
    let v2 = PostVolume {
        format_version: 2,
        overrides: PostOverrides {
            chromatic_aberration: Some(0.004),
            ..PostOverrides::default()
        },
        ..PostVolume::default()
    };
    let upgraded = v2.clone().migrate().expect("v2 upgrades");
    assert_eq!(upgraded.format_version, 3);
    assert_eq!(upgraded.overrides, v2.overrides);
    assert_eq!(upgraded.shape, v2.shape);
    assert_eq!(upgraded.lut, None);
    let fingerprint = post_volume_schema_fingerprint();
    for needle in ["ConvexHull(planes:", "lut:"] {
        assert!(
            fingerprint.contains(needle),
            "fingerprint lacks {needle}: {fingerprint}"
        );
    }
    assert_eq!(
        PostVolumeShape::ConvexHull { planes: Vec::new() }.name(),
        "convex_hull"
    );
}

#[test]
fn scene_files_read_v2_volumes_and_round_trip_v3_hulls() {
    use regia_core::id::Guid;
    use regia_scene::components::{EntityGuid, Name};
    use regia_scene::format::{SCENE_FORMAT_VERSION, SceneFile};
    use regia_scene::spawn::{SnapshotFilter, SpawnOptions, snapshot_scene, spawn_scene};

    fn scene_world() -> World {
        let mut world = World::new();
        world.insert_resource(AppTypeRegistry::default());
        let handle = world.resource::<AppTypeRegistry>().0.clone();
        let mut registry = handle.write();
        regia_scene::components::register_primitives(&mut registry);
        registry.register::<Transform>();
        registry.register::<Name>();
        registry.register::<EntityGuid>();
        regia_render::post_volumes::register_post_volume_types(&mut registry);
        drop(registry);
        world
    }

    let (old, new) = (Guid::new_random(), Guid::new_random());
    let text = format!(
        r#"(
    format_version: {SCENE_FORMAT_VERSION},
    entities: [
        (guid: "{old}", name: "Old", components: {{
            "regia_render::post_volumes::PostVolume": (
                format_version: 2,
                enabled: true,
                shape: Sphere(radius: 3.0),
                priority: 0,
                blend_distance: 1.0,
                weight: 1.0,
                overrides: (saturation: Some(0.5), lens_dirt_intensity: Some(0.2)),
            ),
        }}),
        (guid: "{new}", name: "Hall", components: {{
            "regia_render::post_volumes::PostVolume": (
                format_version: 3,
                enabled: true,
                shape: ConvexHull(planes: [(1.0, 0.0, 0.0, 2.0), (-1.0, 0.0, 0.0, 2.0), (0.0, 1.0, 0.0, 3.0), (0.0, -1.0, 0.0, 0.0), (0.0, 0.0, 1.0, 9.0), (0.0, 0.0, -1.0, 9.0)]),
                priority: 2,
                blend_distance: 1.5,
                weight: 1.0,
                overrides: (exposure_compensation: Some(0.75)),
                lut: Some("looks/hall.cube"),
            ),
        }}),
    ],
    resources: {{}},
)"#
    );
    let file = SceneFile::from_ron_str(&text).expect("scene parses");
    let mut world = scene_world();
    let spawned = spawn_scene(&mut world, &file, &SpawnOptions::default()).expect("spawn");
    assert!(spawned.warnings.is_empty(), "{:?}", spawned.warnings);
    let mut query = world.query::<&PostVolume>();
    let mut volumes: Vec<PostVolume> = query.iter(&world).cloned().collect();
    volumes.sort_by_key(|v| v.format_version);
    let (old, hall) = (&volumes[0], &volumes[1]);
    assert_eq!(old.format_version, 2);
    assert_eq!(old.lut, None, "a v2 volume has no LUT");
    assert_eq!(old.shape, PostVolumeShape::Sphere { radius: 3.0 });
    assert_eq!(old.overrides.lens_dirt_intensity, Some(0.2));
    assert_eq!(old.clone().migrate().map(|v| v.format_version), Ok(3));
    assert_eq!(hall.lut.as_deref(), Some("looks/hall.cube"));
    let PostVolumeShape::ConvexHull { planes } = &hall.shape else {
        panic!("hall is a hull: {:?}", hall.shape); // invariant: test-only
    };
    assert_eq!(planes.len(), 6);
    assert_eq!(hall.weight_at(&GlobalTransform::IDENTITY, [0.0, 1.0, 0.0]), 1.0);

    let saved = snapshot_scene(&mut world, &SnapshotFilter::default())
        .expect("snapshot")
        .to_ron_string()
        .expect("write");
    let reread = SceneFile::from_ron_str(&saved).expect("saved scene parses");
    let mut again = scene_world();
    spawn_scene(&mut again, &reread, &SpawnOptions::default()).expect("respawn");
    let resaved = snapshot_scene(&mut again, &SnapshotFilter::default())
        .expect("snapshot 2")
        .to_ron_string()
        .expect("write 2");
    assert_eq!(resaved, saved, "byte-identical round trip");
}
