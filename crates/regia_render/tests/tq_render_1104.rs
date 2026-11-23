// SPDX-License-Identifier: MPL-2.0
//! Test-gap sweep of 2026-11-04 (lane sprint-testgaps-render-pt): public functions of the newer
//! renderer modules that no test named, and the branches of named ones no test reached. Pure CPU,
//! fixed inputs, no device, no goldens. Each test's doc says what bug it would catch.

use regia_core::math::Vec3;
use regia_render::glass::space::closest_point_on_triangle;
use regia_render::hwrt::bvh::{
    MIN_DIRECTION_COMPONENT, ShadowBvh, inverse_direction, occluded_brute_force, ray_hits_box,
    ray_hits_triangle,
};
use regia_render::hwrt::casters::{
    SceneChange, SignatureBuilder, SunMapReaders, classify_scene_change, coverage_custom_data,
    sun_map_keep_mask,
};
use regia_render::hwrt::variant::SunShadowVariant;
use regia_render::oit::{
    OitFragment, WEIGHT_MAX, WEIGHT_MIN, composite, depth_weight, reactive_from_revealage,
};
use regia_render::post_volumes::{PostOverrides, VolumeContribution, blend_post_volumes};
use regia_render::radiance_cache::RadianceCacheSettings;
use regia_render::shadow::pack_atlas_rects;
use regia_render::view_math::stable_look_up;
use regia_render::visbuffer::{LodView, ROOT_ERROR, meshlet_sub_path, sphere_of_points};
use regia_render::{PostSettings, TonemapOperator, UpscaleQuality};

const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// A fixed-seed generator (numerical recipes LCG), so every run draws the same values.
struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }

    fn point(&mut self, lo: f32, hi: f32) -> [f32; 3] {
        [self.range(lo, hi), self.range(lo, hi), self.range(lo, hi)]
    }
}

// ---------------------------------------------------------------------------------------------
// hwrt::bvh: the ray tests the compute fallback mirrors
// ---------------------------------------------------------------------------------------------

const TRI: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];

/// Catches a Möller-Trumbore port that drops rays on a shared edge (`u + v == 1`) or at a
/// vertex, which would leak light through the seam between two triangles of one quad.
#[test]
fn ray_triangle_counts_edges_and_vertices_as_hits() {
    let down = [0.0, 0.0, -1.0];
    // On the hypotenuse, at the right-angle vertex and at a far vertex.
    for (x, y) in [(0.5, 0.5), (0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
        assert!(
            ray_hits_triangle([x, y, 1.0], down, &TRI, 0.0, 10.0),
            "({x}, {y})"
        );
    }
    // Just outside the hypotenuse.
    assert!(!ray_hits_triangle([0.51, 0.51, 1.0], down, &TRI, 0.0, 10.0));
}

/// Catches a one-sided triangle test (a shadow ray must be blocked by either face) and an
/// interval that includes its open ends (a ray starting on its own surface would self-shadow).
#[test]
fn ray_triangle_is_double_sided_and_the_interval_is_open() {
    assert!(ray_hits_triangle(
        [0.2, 0.2, -1.0],
        [0.0, 0.0, 1.0],
        &TRI,
        0.0,
        10.0
    ));
    // The hit is at t = 1: excluded at either end of the interval.
    assert!(!ray_hits_triangle(
        [0.2, 0.2, 1.0],
        [0.0, 0.0, -1.0],
        &TRI,
        1.0,
        10.0
    ));
    assert!(!ray_hits_triangle(
        [0.2, 0.2, 1.0],
        [0.0, 0.0, -1.0],
        &TRI,
        0.0,
        1.0
    ));
    assert!(ray_hits_triangle(
        [0.2, 0.2, 1.0],
        [0.0, 0.0, -1.0],
        &TRI,
        0.999,
        1.001
    ));
    // Behind the origin.
    assert!(!ray_hits_triangle(
        [0.2, 0.2, 1.0],
        [0.0, 0.0, 1.0],
        &TRI,
        0.0,
        10.0
    ));
}

/// Catches a divide by a zero determinant: a ray in the triangle's plane and a degenerate
/// (zero-area) triangle must both miss instead of hitting with t = NaN or inf.
#[test]
fn ray_triangle_parallel_and_degenerate_miss() {
    assert!(!ray_hits_triangle(
        [-1.0, 0.2, 0.0],
        [1.0, 0.0, 0.0],
        &TRI,
        0.0,
        10.0
    ));
    let sliver = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
    assert!(!ray_hits_triangle(
        [0.5, 0.0, 1.0],
        [0.0, 0.0, -1.0],
        &sliver,
        0.0,
        10.0
    ));
    let point = [[0.3, 0.3, 0.0]; 3];
    assert!(!ray_hits_triangle(
        [0.3, 0.3, 1.0],
        [0.0, 0.0, -1.0],
        &point,
        0.0,
        10.0
    ));
}

/// Catches a slab test that rejects an origin inside the box, accepts a box behind the ray or
/// past `t_max`, or turns NaN on an axis-parallel ray through a face plane.
#[test]
fn ray_box_slab_cases() {
    let (lo, hi) = ([0.0; 3], [1.0; 3]);
    let inv = |d: [f32; 3]| inverse_direction(d);
    assert!(ray_hits_box([0.5; 3], inv([0.3, -0.2, 0.9]), lo, hi, 0.0, 1.0e-3));
    assert!(ray_hits_box(
        [-1.0, 0.5, 0.5],
        inv([1.0, 0.0, 0.0]),
        lo,
        hi,
        0.0,
        10.0
    ));
    // Behind.
    assert!(!ray_hits_box(
        [2.0, 0.5, 0.5],
        inv([1.0, 0.0, 0.0]),
        lo,
        hi,
        0.0,
        10.0
    ));
    // Too short: the box starts at t = 1.
    assert!(!ray_hits_box(
        [-1.0, 0.5, 0.5],
        inv([1.0, 0.0, 0.0]),
        lo,
        hi,
        0.0,
        0.9
    ));
    // Axis-parallel, origin exactly on the y = 0 face plane: the slab of the zero component
    // must not produce NaN and drop the ray.
    assert!(ray_hits_box(
        [-1.0, 0.0, 0.5],
        inv([1.0, 0.0, 0.0]),
        lo,
        hi,
        0.0,
        10.0
    ));
    // Parallel and outside the y slab.
    assert!(!ray_hits_box(
        [-1.0, 1.5, 0.5],
        inv([1.0, 0.0, 0.0]),
        lo,
        hi,
        0.0,
        10.0
    ));
}

/// Catches an `inverse_direction` that lets a zero (or negative zero) component through as an
/// infinite reciprocal: the GPU mirror would then compute `0 × inf = NaN` in the slab test.
#[test]
fn inverse_direction_is_finite_for_zero_components() {
    let inv = inverse_direction([0.0, -0.0, 2.0]);
    assert!(inv.iter().all(|v| v.is_finite()), "{inv:?}");
    assert_eq!(inv[0], 1.0 / MIN_DIRECTION_COMPONENT);
    assert_eq!(inv[1], 1.0 / MIN_DIRECTION_COMPONENT);
    assert_eq!(inv[2], 0.5);
    // Tiny negative components keep their magnitude floor (nudged to the positive floor).
    let tiny = inverse_direction([-1.0e-12, 1.0e-12, -3.0]);
    assert!(tiny.iter().all(|v| v.is_finite()));
}

/// Catches a tree that disagrees with brute force on rays that start inside the bounds, run
/// along axes, or end exactly at a surface (a fixed-seed sweep over a cluttered box).
#[test]
fn bvh_agrees_with_brute_force_on_axis_rays_and_short_segments() {
    let mut rng = Lcg(0x1104_2026);
    let triangles: Vec<[[f32; 3]; 3]> = (0..200)
        .map(|_| {
            let a = rng.point(-4.0, 4.0);
            let b = [
                a[0] + rng.range(-0.6, 0.6),
                a[1] + rng.range(-0.6, 0.6),
                a[2] + rng.range(-0.6, 0.6),
            ];
            let c = [
                a[0] + rng.range(-0.6, 0.6),
                a[1] + rng.range(-0.6, 0.6),
                a[2] + rng.range(-0.6, 0.6),
            ];
            [a, b, c]
        })
        .collect();
    let bvh = ShadowBvh::build(&triangles).expect("bvh builds");
    let axes = [
        [1.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, -1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, -1.0],
    ];
    let mut hits = 0;
    for k in 0..400 {
        let origin = rng.point(-5.0, 5.0);
        let dir = axes[k % axes.len()];
        let t_max = if k % 3 == 0 { rng.range(0.01, 2.0) } else { 100.0 };
        let tree = bvh.occluded(origin, dir, 1.0e-4, t_max);
        let brute = occluded_brute_force(&triangles, origin, dir, 1.0e-4, t_max);
        assert_eq!(tree, brute, "ray {k} from {origin:?} along {dir:?} to {t_max}");
        hits += usize::from(tree);
    }
    assert!(
        hits > 20 && hits < 380,
        "the sweep should mix hits and misses: {hits}"
    );
}

// ---------------------------------------------------------------------------------------------
// hwrt::casters and hwrt::variant: cache keys and the variant table
// ---------------------------------------------------------------------------------------------

fn moved(x: f32) -> [[f32; 4]; 4] {
    let mut m = IDENTITY;
    m[3][0] = x;
    m
}

/// Catches a scene signature that is order-blind: swapping which mesh sits in which slot changes
/// the instance-to-slot mapping the top level holds, so it must rebuild, not refit.
#[test]
fn swapping_slots_rebuilds() {
    let sign = |slots: &[(u64, u64)]| {
        let mut b = SignatureBuilder::new();
        for &(mesh, generation) in slots {
            b.slot(mesh, generation, 0, &IDENTITY);
        }
        b.finish()
    };
    let ab = sign(&[(1, 1), (2, 1)]);
    let ba = sign(&[(2, 1), (1, 1)]);
    assert_eq!(classify_scene_change(Some(ab), ba), SceneChange::Rebuild);
    // Mesh and generation are separate words: (1, 2) is not (2, 1).
    assert_ne!(sign(&[(1, 2)]).structure, sign(&[(2, 1)]).structure);
    // A source counter mixed into the structure alone rebuilds without touching transforms.
    let mut b = SignatureBuilder::new();
    b.slot(1, 1, 0, &IDENTITY);
    b.slot(2, 1, 0, &IDENTITY);
    b.structure(7);
    let counted = b.finish();
    assert_eq!(counted.transforms, ab.transforms);
    assert_eq!(classify_scene_change(Some(ab), counted), SceneChange::Rebuild);
}

/// Catches a transform hash that compares floats by value: `-0.0` and `0.0` must count as a
/// move (refit, cheap) and never as a structural change (rebuild, expensive), and the signature
/// of a fixed input must not depend on the run (FNV-1a over the bits, pinned here).
#[test]
fn signatures_are_bitwise_and_stable() {
    let mut a = SignatureBuilder::new();
    a.slot(5, 1, 0, &moved(0.0));
    let mut b = SignatureBuilder::new();
    b.slot(5, 1, 0, &moved(-0.0));
    let (a, b) = (a.finish(), b.finish());
    assert_eq!(classify_scene_change(Some(a), b), SceneChange::Refit);
    // The empty scene: FNV-1a offset basis folded with a slot count of 0.
    let empty = SignatureBuilder::new().finish();
    let expected = 0xcbf2_9ce4_8422_2325u64.wrapping_mul(0x0000_0100_0000_01b3);
    assert_eq!(empty.structure, expected);
    assert_eq!(empty.transforms, 0xcbf2_9ce4_8422_2325);
    assert_eq!(classify_scene_change(None, empty), SceneChange::Rebuild);
}

/// Catches a coverage packer that turns NaN or a negative coverage into garbage custom data, or
/// rounds the 8-bit step the wrong way.
#[test]
fn coverage_custom_data_edges() {
    assert_eq!(coverage_custom_data(f32::NAN), 0);
    assert_eq!(coverage_custom_data(-3.0), 0);
    assert_eq!(coverage_custom_data(f32::INFINITY), 255);
    assert_eq!(coverage_custom_data(f32::NEG_INFINITY), 0);
    assert_eq!(coverage_custom_data(1.0 / 255.0), 1);
    assert_eq!(coverage_custom_data(0.4 / 255.0), 0);
    assert_eq!(coverage_custom_data(254.6 / 255.0), 255);
}

/// Catches a keep mask that shifts by an unchecked cascade index (`1 << 9` overflows a `u8`):
/// a composite cascade past the last one clamps to bit 7, and no reader keeps nothing.
#[test]
fn sun_map_keep_mask_clamps_the_cascade() {
    let fog = |c| SunMapReaders {
        fog_cascade: Some(c),
        ..SunMapReaders::default()
    };
    assert_eq!(
        sun_map_keep_mask(SunShadowVariant::ComputeFallback, true, fog(9)),
        0x80
    );
    assert_eq!(
        sun_map_keep_mask(SunShadowVariant::ComputeFallback, true, fog(255)),
        0x80
    );
    assert_eq!(
        sun_map_keep_mask(SunShadowVariant::ComputeFallback, true, fog(0)),
        0x01
    );
    // Not applied: every cascade stays, whatever reads them.
    assert_eq!(
        sun_map_keep_mask(SunShadowVariant::ComputeFallback, false, SunMapReaders::default()),
        u8::MAX
    );
    // The screen trace does not trace geometry: the maps stay.
    assert_eq!(
        sun_map_keep_mask(SunShadowVariant::ScreenTrace, true, SunMapReaders::default()),
        u8::MAX
    );
}

/// Catches a variant table whose report ids or names do not round-trip (a report naming the
/// wrong variant), or whose pipeline key bit disagrees with the variants that trace rays.
#[test]
fn sun_shadow_variant_ids_and_names_round_trip() {
    let mut seen = std::collections::HashSet::new();
    for v in SunShadowVariant::ALL {
        assert_eq!(SunShadowVariant::from_key_bits(v.key_bits()), Some(v));
        assert!(!v.as_str().is_empty());
        assert!(seen.insert(v.key_bits()));
        assert_eq!(v.forward_key_bit(), v.traces_rays());
        assert_eq!(v.forward_shader_defs().is_empty(), !v.traces_rays());
        assert_eq!(v.trace_shader().is_some(), v != SunShadowVariant::ShadowMap);
        assert!(v.runs_on(v.required_features()));
    }
    for bits in 4..=u8::MAX {
        assert_eq!(SunShadowVariant::from_key_bits(bits), None, "{bits}");
    }
    assert!(!SunShadowVariant::RayQuery.runs_on(wgpu::Features::empty()));
}

// ---------------------------------------------------------------------------------------------
// glass::space and view_math
// ---------------------------------------------------------------------------------------------

/// Catches a Voronoi-region slip in the closest-point routine: for fixed-seed points around a
/// triangle, the answer reconstructs from its barycentrics (which sum to 1 and are all in
/// `0..=1`) and is never farther than any of a grid of points on the triangle.
#[test]
fn closest_point_on_triangle_is_the_closest() {
    let (a, b, c) = (
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(2.0, 0.0, 0.0),
        Vec3::new(0.5, 1.5, 0.3),
    );
    let mut rng = Lcg(42);
    let samples: Vec<Vec3> = (0..=20)
        .flat_map(|i| (0..=(20 - i)).map(move |j| (i, j)))
        .map(|(i, j)| {
            let (u, v) = (i as f32 / 20.0, j as f32 / 20.0);
            a * (1.0 - u - v) + b * u + c * v
        })
        .collect();
    for _ in 0..300 {
        let p = Vec3::from_array(rng.point(-2.0, 3.0));
        let (q, bary) = closest_point_on_triangle(p, a, b, c);
        assert!((bary.x + bary.y + bary.z - 1.0).abs() < 1e-5, "{bary:?}");
        assert!(
            bary.to_array().iter().all(|w| (-1e-6..=1.0 + 1e-6).contains(w)),
            "{bary:?}"
        );
        let rebuilt = a * bary.x + b * bary.y + c * bary.z;
        assert!((rebuilt - q).length() < 1e-4, "{rebuilt:?} vs {q:?}");
        let best = samples.iter().map(|s| (*s - p).length()).fold(f32::MAX, f32::min);
        assert!(
            (q - p).length() <= best + 1e-4,
            "{p:?}: {} > {best}",
            (q - p).length()
        );
    }
}

/// Catches a closest-point routine that divides by zero on degenerate triangles (a point and a
/// segment), which a cracked-glass stamp on a collapsed mesh triangle would feed it.
#[test]
fn closest_point_on_degenerate_triangles_is_finite() {
    let p = Vec3::new(0.3, 0.7, -0.2);
    let point = Vec3::new(1.0, 1.0, 1.0);
    let (q, bary) = closest_point_on_triangle(p, point, point, point);
    assert_eq!(q, point);
    assert!(bary.is_finite());
    let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::X * 2.0);
    for p in [
        Vec3::new(0.5, 1.0, 0.0),
        Vec3::new(1.5, -1.0, 0.0),
        Vec3::new(3.0, 0.0, 1.0),
    ] {
        let (q, bary) = closest_point_on_triangle(p, a, b, c);
        assert!(q.is_finite() && bary.is_finite(), "{p:?}: {q:?} {bary:?}");
        assert!(
            q.y.abs() < 1e-6 && q.z.abs() < 1e-6 && (0.0..=2.0).contains(&q.x),
            "{q:?}"
        );
    }
}

/// Catches a look-at up vector that is parallel to the view direction (a NaN view matrix) at
/// the poles, for a non-finite `up`, or for an `up` along X or Z.
#[test]
fn stable_look_up_is_never_parallel_to_forward() {
    assert_eq!(stable_look_up(Vec3::NEG_Y, Vec3::Y), Vec3::NEG_Z);
    assert_eq!(stable_look_up(Vec3::Y, Vec3::Y), Vec3::Z);
    assert_eq!(stable_look_up(Vec3::new(0.0, -3.0, 0.0), Vec3::NEG_Y), Vec3::Z);
    assert_eq!(stable_look_up(Vec3::Z, Vec3::new(f32::NAN, 1.0, 0.0)), Vec3::Y);
    assert_eq!(stable_look_up(Vec3::Z, Vec3::ZERO), Vec3::Y);
    assert_eq!(stable_look_up(Vec3::ZERO, Vec3::X), Vec3::X);
    assert_eq!(stable_look_up(Vec3::splat(f32::INFINITY), Vec3::Y), Vec3::Y);
    for (forward, up) in [
        (Vec3::X, Vec3::X),
        (Vec3::NEG_X, Vec3::X),
        (Vec3::Z, Vec3::NEG_Z),
        (Vec3::new(1.0, 1.0, 1.0), Vec3::new(2.0, 2.0, 2.0)),
        (Vec3::new(0.0, -1.0, 1.0e-6), Vec3::Y),
    ] {
        let chosen = stable_look_up(forward, up);
        let cross = forward.normalize().cross(chosen.normalize());
        assert!(cross.length() > 0.1, "{forward:?} {up:?} -> {chosen:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// shadow atlas, visbuffer LOD helpers, upscaler modes
// ---------------------------------------------------------------------------------------------

/// Catches a shelf packer that overlaps tiles, lets one stick out of the atlas, accepts a zero
/// size, or panics on `x + size` overflow for an atlas edge near `u32::MAX` (defect fixed in this
/// lane: the sums saturate).
#[test]
fn shelf_pack_bounds_and_overflow() {
    assert_eq!(pack_atlas_rects(&[4], 0), None);
    assert_eq!(pack_atlas_rects(&[0], 16), None);
    assert_eq!(pack_atlas_rects(&[17], 16), None);
    assert_eq!(pack_atlas_rects(&[], 16), Some(Vec::new()));
    let rects = pack_atlas_rects(&[8, 8, 8, 8], 16).expect("four quarters fit");
    let corners: Vec<(u32, u32)> = rects.iter().map(|r| (r.x, r.y)).collect();
    assert_eq!(corners, vec![(0, 0), (8, 0), (0, 8), (8, 8)]);
    assert_eq!(pack_atlas_rects(&[8, 8, 8, 8, 8], 16), None);
    // Mixed sizes: every rect inside, no two overlapping.
    let sizes = [8, 4, 4, 2, 2, 2, 2, 8, 1, 1];
    let rects = pack_atlas_rects(&sizes, 32).expect("fits");
    for (i, a) in rects.iter().enumerate() {
        assert!(a.x + a.size <= 32 && a.y + a.size <= 32);
        assert_eq!(a.size, sizes[i]);
        for b in &rects[i + 1..] {
            let apart =
                a.x + a.size <= b.x || b.x + b.size <= a.x || a.y + a.size <= b.y || b.y + b.size <= a.y;
            assert!(apart, "{a:?} overlaps {b:?}");
        }
    }
    // Near the top of u32: must answer, not overflow.
    let big = u32::MAX;
    assert_eq!(pack_atlas_rects(&[big, 1], big), None);
    let half = big / 2 + 1;
    assert_eq!(pack_atlas_rects(&[half, half], big), None);
}

/// Catches a meshlet path mapper that accepts paths that are not mesh sub-assets (a `meshes/`
/// prefix, a bare `mesh`), so the loader asks the cache for a sub-asset nobody cooked.
#[test]
fn meshlet_sub_paths() {
    assert_eq!(
        meshlet_sub_path("mesh/root/primitive_0").as_deref(),
        Some("meshlets/root/primitive_0")
    );
    assert_eq!(meshlet_sub_path("mesh/").as_deref(), Some("meshlets/"));
    assert_eq!(meshlet_sub_path("meshes/root/primitive_0"), None);
    assert_eq!(meshlet_sub_path("mesh"), None);
    assert_eq!(meshlet_sub_path(""), None);
    assert_eq!(meshlet_sub_path("texture/mesh/x"), None);
}

/// Catches a bounding sphere that misses a point (culling a visible cluster), is not zero for an
/// empty set, or lets a NaN point poison the centre.
#[test]
fn sphere_of_points_contains_every_point() {
    assert_eq!(sphere_of_points(std::iter::empty()), [0.0; 4]);
    assert_eq!(
        sphere_of_points(std::iter::once([1.0, 2.0, 3.0])),
        [1.0, 2.0, 3.0, 0.0]
    );
    let mut rng = Lcg(7);
    let points: Vec<[f32; 3]> = (0..257).map(|_| rng.point(-50.0, 80.0)).collect();
    let s = sphere_of_points(points.iter().copied());
    for p in &points {
        let d = ((p[0] - s[0]).powi(2) + (p[1] - s[1]).powi(2) + (p[2] - s[2]).powi(2)).sqrt();
        assert!(d <= s[3] * (1.0 + 1e-6), "{p:?} outside {s:?}");
    }
    let with_nan = [[0.0, 0.0, 0.0], [f32::NAN, 0.0, 0.0], [2.0, 0.0, 0.0]];
    let s = sphere_of_points(with_nan.iter().copied());
    assert!(s.iter().all(|v| v.is_finite()), "{s:?}");
    assert_eq!(s, [1.0, 0.0, 0.0, 1.0]);
}

/// Catches a LOD metric that goes negative or infinite with the eye inside a cluster's sphere
/// (the cut would then draw the coarsest level up close), and a root error that is not infinite.
#[test]
fn projected_error_inside_the_sphere_uses_the_near_distance() {
    let view = LodView {
        eye: [0.0; 3],
        pixels_per_unit: 1000.0,
        threshold_px: 1.0,
        near: 0.1,
    };
    let inside = view.projected_error([0.5, 0.0, 0.0, 2.0], 0.01);
    assert!((inside - 0.01 * 1000.0 / 0.1).abs() < 1e-3, "{inside}");
    let far = view.projected_error([100.0, 0.0, 0.0, 1.0], 0.01);
    assert!((far - 0.01 * 1000.0 / 99.0).abs() < 1e-4, "{far}");
    assert!(inside > far);
    assert_eq!(view.projected_error([0.0; 4], ROOT_ERROR), f32::INFINITY);
    // A NaN error is never "fine enough".
    let nan = view.projected_error([10.0, 0.0, 0.0, 1.0], f32::NAN);
    assert!(nan.partial_cmp(&view.threshold_px) != Some(std::cmp::Ordering::Less));
}

/// Catches an upscale mode ladder that loops, skips a mode, or gives a dynamic-resolution range
/// whose floor is above its ceiling; and render sizes that grow when the mode goes down.
#[test]
fn upscale_modes_step_down_monotonically() {
    let mut mode = UpscaleQuality::Native;
    let mut seen = vec![mode];
    for _ in 0..UpscaleQuality::ALL.len() {
        let next = mode.next_lower();
        if next == mode {
            break;
        }
        seen.push(next);
        mode = next;
    }
    assert_eq!(seen, UpscaleQuality::ALL.to_vec());
    assert_eq!(
        UpscaleQuality::UltraPerformance.next_lower(),
        UpscaleQuality::UltraPerformance
    );
    for m in UpscaleQuality::ALL {
        let (lo, hi) = m.permille_range();
        assert!(lo <= hi, "{m:?}: {lo}..{hi}");
        let (num, den) = m.ratio_fraction();
        assert!(num >= den && den > 0);
        assert!((m.ratio() - num as f32 / den as f32).abs() < 1e-6);
        for display in [1u32, 2, 3, 7, 719, 1080, 1440, 2160, 4321, u32::MAX] {
            let r = m.render_axis(display);
            assert!(r >= 1 && r <= display, "{m:?} at {display}: {r}");
            assert!(m.next_lower().render_axis(display) <= r);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Radiance cache settings
// ---------------------------------------------------------------------------------------------

/// Catches `clamped` passing NaN through (`f32::clamp` keeps NaN): the settings would then never
/// compare equal to themselves, so the renderer would rebuild the cache's planner and buffers on
/// every `set_radiance_cache_settings` call. Defect fixed in this lane (NaN reads as the default).
#[test]
fn radiance_cache_clamp_replaces_nan_and_is_idempotent() {
    let nan = RadianceCacheSettings {
        spacing_scale: f32::NAN,
        level_ratio: f32::NAN,
        ray_scale: f32::NAN,
        strength: f32::NAN,
        miss_cap: f32::NAN,
        darken_limit: f32::NAN,
        ..RadianceCacheSettings::default()
    };
    let clamped = nan.clamped();
    assert_eq!(clamped, RadianceCacheSettings::default());
    assert_eq!(clamped.clamped(), clamped);
    let inf = RadianceCacheSettings {
        spacing_scale: f32::INFINITY,
        level_ratio: f32::NEG_INFINITY,
        ..RadianceCacheSettings::default()
    }
    .clamped();
    assert_eq!((inf.spacing_scale, inf.level_ratio), (8.0, 1.5));
    assert_eq!(inf, inf.clamped());
}

/// Catches a size computation that overflows or drops a level: the largest layout the clamp
/// allows, its budget clamp to every cell, and the spacing ladder of each level.
#[test]
fn radiance_cache_sizes_at_the_clamp_limits() {
    let max = RadianceCacheSettings {
        levels: 99,
        dims: [1000, 0, 64],
        budget_cells: u32::MAX,
        ..RadianceCacheSettings::default()
    }
    .clamped();
    assert_eq!(max.levels, 4);
    assert_eq!(max.dims, [64, 4, 64]);
    assert_eq!(max.cells_per_level(), 64 * 4 * 64);
    assert_eq!(max.cells(), 4 * 64 * 4 * 64);
    assert_eq!(u64::from(max.budget_cells), max.cells());
    assert_eq!(max.cell_bytes(), max.cells() * 7 * 4);
    let min = RadianceCacheSettings {
        levels: 0,
        budget_cells: 0,
        ..RadianceCacheSettings::default()
    }
    .clamped();
    assert_eq!((min.levels, min.budget_cells), (1, 1));
    let d = RadianceCacheSettings::default();
    assert_eq!(d.spacing_of(0, 2.0), 2.0 * d.spacing_scale);
    for level in 1..4 {
        let ratio = d.spacing_of(level, 2.0) / d.spacing_of(level - 1, 2.0);
        assert!((ratio - d.level_ratio).abs() < 1e-5, "{level}: {ratio}");
    }
}

// ---------------------------------------------------------------------------------------------
// Post volumes: the branches the format tests did not reach
// ---------------------------------------------------------------------------------------------

fn contribution(weight: f32, key: u64, overrides: PostOverrides) -> VolumeContribution {
    VolumeContribution {
        weight,
        priority: 0,
        key,
        overrides,
    }
}

/// Catches an infinite white balance that blends toward the warm end of the fit: the module
/// promises that a value that is not finite counts as unset. Defect fixed in this lane (the
/// kelvin filter checked `> 0` only, so `+inf` read as 25 000 K).
#[test]
fn an_infinite_white_balance_counts_as_unset() {
    let base = PostSettings::default();
    for kelvin in [f32::INFINITY, f32::NAN, f32::NEG_INFINITY, 0.0, -10.0] {
        let mut post = base.clone();
        let wb = PostOverrides {
            white_balance_kelvin: Some(kelvin),
            ..PostOverrides::default()
        };
        blend_post_volumes(&mut post, &mut [contribution(1.0, 1, wb)]);
        assert_eq!(
            post.grade.white_balance_kelvin, base.grade.white_balance_kelvin,
            "{kelvin} must leave the base white balance"
        );
    }
}

/// Catches a tonemap switch that reads one volume's weight instead of the tier's: two volumes of
/// 0.3 together pass the half-weight rule, and the heavier one's operator wins; at equal weights
/// the lowest key wins whatever the input order.
#[test]
fn the_tonemap_switch_sums_the_tier_and_ties_go_to_the_lowest_key() {
    let op = |t| PostOverrides {
        tonemap: Some(t),
        ..PostOverrides::default()
    };
    let base = PostSettings {
        tonemap: TonemapOperator::AgX,
        ..PostSettings::default()
    };
    let mut post = base.clone();
    blend_post_volumes(
        &mut post,
        &mut [
            contribution(0.25, 1, op(TonemapOperator::Neutral)),
            contribution(0.3, 2, op(TonemapOperator::AcesFitted)),
        ],
    );
    assert_eq!(post.tonemap, TonemapOperator::AcesFitted);
    for order in [[1u64, 2], [2, 1]] {
        let mut post = base.clone();
        let mut list: Vec<VolumeContribution> = order
            .iter()
            .map(|&k| {
                let t = if k == 1 {
                    TonemapOperator::Neutral
                } else {
                    TonemapOperator::AcesFitted
                };
                contribution(0.4, k, op(t))
            })
            .collect();
        blend_post_volumes(&mut post, &mut list);
        assert_eq!(post.tonemap, TonemapOperator::Neutral, "order {order:?}");
    }
    // A volume without a tonemap does not count toward the half.
    let mut post = base.clone();
    blend_post_volumes(
        &mut post,
        &mut [
            contribution(0.3, 1, op(TonemapOperator::Neutral)),
            contribution(1.0, 2, PostOverrides::default()),
        ],
    );
    assert_eq!(post.tonemap, TonemapOperator::AgX);
}

/// Catches a priority sort that overflows or reverses at the `i32` extremes.
#[test]
fn priority_extremes_order_the_tiers() {
    let sat = |s| PostOverrides {
        saturation: Some(s),
        ..PostOverrides::default()
    };
    let mut post = PostSettings::default();
    let mut list = [
        VolumeContribution {
            priority: i32::MAX,
            ..contribution(1.0, 1, sat(0.25))
        },
        VolumeContribution {
            priority: i32::MIN,
            ..contribution(1.0, 2, sat(1.75))
        },
    ];
    blend_post_volumes(&mut post, &mut list);
    assert_eq!(post.saturation, 0.25);
}

// ---------------------------------------------------------------------------------------------
// Weighted blended OIT: the non-finite and limit inputs
// ---------------------------------------------------------------------------------------------

/// Catches a depth weight that leaves its bounds at the extremes (0, negative, infinite
/// distance), which would overflow the half-float accumulation target or zero a layer.
#[test]
fn oit_depth_weight_stays_in_bounds() {
    assert_eq!(depth_weight(0.0), WEIGHT_MAX);
    assert_eq!(depth_weight(f32::INFINITY), WEIGHT_MIN);
    assert_eq!(depth_weight(-5.0), depth_weight(5.0));
    assert_eq!(depth_weight(1.0e30), WEIGHT_MIN);
    let mut last = WEIGHT_MAX;
    for i in 0..2000 {
        let w = depth_weight(i as f32 * 0.5);
        assert!((WEIGHT_MIN..=WEIGHT_MAX).contains(&w));
        assert!(w <= last, "the weight rises at {i}");
        last = w;
    }
}

/// Catches a composite that changes the background with no layers, lets a NaN color channel
/// poison the pixel, or shows the background through a fully opaque layer.
#[test]
fn oit_composite_limits() {
    let background = [0.2, 0.4, 0.6];
    assert_eq!(composite(background, &[]), background);
    let nan = OitFragment {
        color: [f32::NAN, 0.5, 0.5],
        alpha: 0.5,
        premultiplied: false,
        distance: 3.0,
    };
    let out = composite(background, &[nan]);
    assert!(out.iter().all(|v| v.is_finite()), "{out:?}");
    assert!(
        (out[0] - background[0] * 0.5).abs() < 1e-5,
        "a NaN channel adds no light: {out:?}"
    );
    let opaque = OitFragment {
        color: [1.0, 0.0, 0.0],
        alpha: 1.0,
        premultiplied: false,
        distance: 2.0,
    };
    let a = composite([0.0; 3], &[opaque]);
    let b = composite([100.0; 3], &[opaque]);
    assert_eq!(a, b, "the background shows through an opaque layer");
    assert!((a[0] - 1.0).abs() < 1e-5);
    // A huge but finite color stays finite through the half-float weight cap.
    let bright = OitFragment {
        color: [1.0e30, 0.0, 0.0],
        ..opaque
    };
    let (accum, _) = bright.accumulate();
    assert!(accum.iter().all(|v| v.is_finite() && *v <= 65504.0), "{accum:?}");
    assert_eq!(reactive_from_revealage(0.0), regia_render::oit::REACTIVE_SCALE);
    assert_eq!(reactive_from_revealage(2.0), 0.0);
}
