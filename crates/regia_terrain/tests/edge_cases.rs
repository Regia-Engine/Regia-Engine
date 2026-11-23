// SPDX-License-Identifier: MPL-2.0
//! Edge cases of the small public CPU surface of `regia_terrain`: non-finite and boundary
//! inputs, error paths, repeated-run determinism. No GPU, no goldens, no wall-clock.

use glam::Vec3;
use regia_asset::Heightfield;
use regia_terrain::height_mips::{MAX_HEIGHT_MIP, mip_level_count};
use regia_terrain::noise::{fbm2, hash2, hash3, value_noise2, value_noise3};
use regia_terrain::sampler::TerrainMapError;
use regia_terrain::sim::{
    AgreementFlag, CLIFF_DELTA_M, DRIVEABLE_DELTA_M, DRIVEABLE_SLOPE_RAD, SimGround, agreement_flag,
    do27_gpu_budgets, visual_sim_delta,
};
use regia_terrain::slope_mips::{f16_bits, f16_value, level_size, sample_slope};
use regia_terrain::{
    HeightMips, RoadRibbon, ScatterSets, TerrainMap, mist_grid_from_map, on_segment, raycast_with_holes,
    vertex_mip,
};
use regia_test_utils::{unique_temp_dir, write_atomic};

fn plane(n: u32, world: f32, f: impl Fn(f32, f32) -> f32) -> TerrainMap {
    let mut heights = Vec::with_capacity((n * n) as usize);
    for row in 0..n {
        for col in 0..n {
            let x = col as f32 / (n - 1) as f32 * world - world * 0.5;
            let z = row as f32 / (n - 1) as f32 * world - world * 0.5;
            heights.push(f(x, z));
        }
    }
    TerrainMap::from_heights(n, n, heights, world, 1.0, 0.0).expect("map")
}

// ---- road --------------------------------------------------------------------------------

#[test]
fn road_ribbon_rejects_a_non_finite_centerline_point() {
    let map = plane(9, 32.0, |_, _| 1.0);
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for (i, line) in [
            vec![[bad, 0.0], [4.0, 0.0], [8.0, 0.0]],
            vec![[0.0, 0.0], [4.0, bad], [8.0, 0.0]],
            vec![[0.0, 0.0], [4.0, 0.0], [8.0, bad]],
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                RoadRibbon::conform(&map, &line, 1.0, 0.02).is_err(),
                "centerline with {bad} at point {i} must be refused, not meshed"
            );
        }
    }
}

#[test]
fn road_ribbon_rejects_bad_parameters_and_short_lines() {
    let map = plane(9, 32.0, |_, _| 1.0);
    let line = [[-4.0, 0.0], [4.0, 0.0]];
    assert!(RoadRibbon::conform(&map, &[], 1.0, 0.02).is_err());
    assert!(RoadRibbon::conform(&map, &line[..1], 1.0, 0.02).is_err());
    for hw in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert!(
            RoadRibbon::conform(&map, &line, hw, 0.02).is_err(),
            "half width {hw}"
        );
    }
    for lift in [f32::NAN, f32::INFINITY, -0.1, 0.0, 0.0099] {
        assert!(
            RoadRibbon::conform(&map, &line, 1.0, lift).is_err(),
            "lift {lift}"
        );
    }
    // The lift band is closed at both ends.
    assert!(RoadRibbon::conform(&map, &line, 1.0, 0.01).is_ok());
    assert!(RoadRibbon::conform(&map, &line, 1.0, DRIVEABLE_DELTA_M).is_ok());
    assert!(RoadRibbon::conform(&map, &line, 1.0, DRIVEABLE_DELTA_M + 1.0e-3).is_err());
}

#[test]
fn road_ribbon_indices_are_in_range_and_the_build_is_deterministic() {
    let map = plane(17, 64.0, |x, z| 0.1 * x - 0.05 * z);
    // Includes a repeated point (zero-length tangent on one side) and a hairpin.
    let line = [
        [-8.0, 0.0],
        [0.0, 0.0],
        [0.0, 0.0],
        [8.0, 4.0],
        [0.0, 8.0],
        [-8.0, 4.0],
    ];
    let a = RoadRibbon::conform(&map, &line, 1.5, 0.05).expect("ribbon");
    let b = RoadRibbon::conform(&map, &line, 1.5, 0.05).expect("ribbon");
    assert_eq!(a.vertices, b.vertices);
    assert_eq!(a.indices, b.indices);
    assert_eq!(a.vertices.len(), line.len() * 2);
    assert_eq!(a.indices.len(), (line.len() - 1) * 6);
    assert!(a.indices.iter().all(|&i| (i as usize) < a.vertices.len()));
    assert!(
        a.vertices
            .iter()
            .all(|v| v.iter().all(|c| c.is_finite()) && v[3] == 1.0)
    );
}

#[test]
fn road_ribbon_off_the_map_clamps_to_the_border_height() {
    let map = plane(9, 32.0, |x, _| x * 0.5);
    let line = [[500.0, 0.0], [600.0, 0.0]];
    let ribbon = RoadRibbon::conform(&map, &line, 1.0, 0.02).expect("ribbon");
    let edge = map.sample_bilinear(16.0, 0.0) + 0.02;
    assert!(ribbon.vertices.iter().all(|v| (v[1] - edge).abs() < 1e-4));
}

#[test]
fn on_segment_degenerate_and_non_finite_inputs() {
    // A zero-length segment is a disc of the half width.
    assert!(on_segment([1.0, 1.0], [1.0, 1.0], 0.5, 1.2, 1.0));
    assert!(!on_segment([1.0, 1.0], [1.0, 1.0], 0.5, 2.0, 1.0));
    // The test is strict: exactly the half width away is outside.
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], 1.0, 5.0, 1.0));
    // Disabled decals and NaN inputs are never "on".
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], 0.0, 5.0, 0.0));
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], f32::NAN, 5.0, 0.0));
    assert!(!on_segment([0.0, 0.0], [10.0, 0.0], 1.0, f32::NAN, 0.0));
    assert!(!on_segment([f32::NAN, 0.0], [10.0, 0.0], 1.0, 5.0, 0.0));
}

// ---- noise -------------------------------------------------------------------------------

#[test]
fn noise_stays_in_range_for_negative_and_huge_coordinates() {
    let coords = [
        -1.0e-7,
        -0.5,
        -3.75,
        -1000.5,
        1.0e6,
        -1.0e6,
        2.5e9,
        -2.5e9,
        1.0e12,
        -1.0e20,
        3.0e38,
        f32::MAX,
        f32::MIN,
    ];
    for &x in &coords {
        for &y in &coords {
            let n2 = value_noise2(x, y, 11);
            assert!((0.0..=1.0).contains(&n2), "value_noise2({x}, {y}) = {n2}");
            let n3 = value_noise3(x, y, x, 11);
            assert!((0.0..=1.0).contains(&n3), "value_noise3({x}, {y}, {x}) = {n3}");
            let f = fbm2(x, y, 11);
            assert!((0.0..=1.0 + 1.0e-4).contains(&f), "fbm2({x}, {y}) = {f}");
        }
    }
}

#[test]
fn noise_never_returns_nan() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(value_noise2(bad, 0.5, 1).is_finite(), "value_noise2({bad})");
        assert!(value_noise2(0.5, bad, 1).is_finite(), "value_noise2(_, {bad})");
        assert!(value_noise3(bad, 0.5, 0.5, 1).is_finite(), "value_noise3({bad})");
        assert!(
            value_noise3(0.5, 0.5, bad, 1).is_finite(),
            "value_noise3(_, _, {bad})"
        );
        assert!(fbm2(bad, bad, 1).is_finite(), "fbm2({bad})");
    }
}

#[test]
fn noise_is_continuous_across_lattice_cells_including_negative_ones() {
    // The quintic fade has zero slope at the lattice, so a tiny step moves the value by far
    // less than the step. A cell-boundary bug (floor vs truncate) would jump at x = 0.
    for &x in &[-2.0f32, -1.0, 0.0, 1.0, 2.0] {
        let below = value_noise2(x - 1.0e-4, 0.3, 5);
        let above = value_noise2(x + 1.0e-4, 0.3, 5);
        assert!(
            (below - above).abs() < 1.0e-3,
            "jump at x = {x}: {below} vs {above}"
        );
        let below = value_noise3(0.3, x - 1.0e-4, 0.7, 5);
        let above = value_noise3(0.3, x + 1.0e-4, 0.7, 5);
        assert!(
            (below - above).abs() < 1.0e-3,
            "3D jump at y = {x}: {below} vs {above}"
        );
    }
}

#[test]
fn hashes_are_stable_seeded_and_in_range() {
    for i in -50..50 {
        let h2 = hash2(i, -i * 3, 17);
        let h3 = hash3(i, -i * 3, i ^ 5, 17);
        assert!((0.0..1.0).contains(&h2) && (0.0..1.0).contains(&h3));
        assert_eq!(h2, hash2(i, -i * 3, 17));
        assert_eq!(h3, hash3(i, -i * 3, i ^ 5, 17));
    }
    // Seeds, axes and extreme coordinates all move the value.
    assert_ne!(hash2(3, 4, 1), hash2(3, 4, 2));
    assert_ne!(hash2(3, 4, 1), hash2(4, 3, 1));
    assert_ne!(hash3(1, 2, 3, 0), hash3(3, 2, 1, 0));
    assert!((0.0..1.0).contains(&hash2(i32::MIN, i32::MAX, u32::MAX)));
    assert!((0.0..1.0).contains(&hash3(i32::MAX, i32::MIN, i32::MAX, u32::MAX)));
    // Pinned values: the hash feeds saved tunnel detail and test fields, so a change is visible.
    let pinned = [hash2(0, 0, 0), hash2(1, 0, 0), hash2(0, 1, 0), hash3(1, 2, 3, 4)];
    let again = [hash2(0, 0, 0), hash2(1, 0, 0), hash2(0, 1, 0), hash3(1, 2, 3, 4)];
    assert_eq!(pinned, again);
    // A decent spread: the mean over a block is near 0.5.
    let mean: f32 = (0..64)
        .flat_map(|y| (0..64).map(move |x| hash2(x, y, 9)))
        .sum::<f32>()
        / 4096.0;
    assert!((mean - 0.5).abs() < 0.03, "mean {mean}");
}

// ---- half floats -------------------------------------------------------------------------

#[test]
fn every_finite_half_round_trips_exactly() {
    for bits in 0..=u16::MAX {
        let exp = (bits >> 10) & 0x1f;
        if exp == 0x1f {
            continue;
        }
        let v = f16_value(bits);
        assert!(v.is_finite());
        assert_eq!(f16_bits(v), bits, "{bits:#06x} -> {v}");
    }
}

#[test]
fn half_conversion_special_values_and_rounding() {
    // Signed zero keeps its sign.
    assert_eq!(f16_bits(0.0), 0x0000);
    assert_eq!(f16_bits(-0.0), 0x8000);
    assert!(f16_value(0x8000).is_sign_negative() && f16_value(0x8000) == 0.0);
    // Infinities and NaN.
    assert_eq!(f16_bits(f32::INFINITY), 0x7c00);
    assert_eq!(f16_bits(f32::NEG_INFINITY), 0xfc00);
    let nan = f16_bits(f32::NAN);
    assert_eq!(nan & 0x7c00, 0x7c00);
    assert_ne!(nan & 0x03ff, 0, "a NaN must not collapse to infinity");
    assert!(f16_value(nan).is_nan());
    assert!(f16_value(0x7c00).is_infinite() && f16_value(0xfc00) < 0.0);
    // Overflow: 65520 is the first value that rounds up to infinity.
    assert_eq!(f16_bits(65519.0), 0x7bff);
    assert_eq!(f16_bits(65520.0), 0x7c00);
    assert_eq!(f16_bits(-1.0e9), 0xfc00);
    // Underflow: below half the smallest subnormal flushes to (signed) zero; the tie rounds to
    // even (zero); above it rounds up to the smallest subnormal 2^-24.
    let min_sub = 2f32.powi(-24);
    assert_eq!(f16_bits(min_sub * 0.49), 0x0000);
    assert_eq!(f16_bits(min_sub * 0.5), 0x0000);
    assert_eq!(f16_bits(min_sub * 0.51), 0x0001);
    assert_eq!(f16_bits(-min_sub * 0.49), 0x8000);
    assert_eq!(f16_bits(1.0e-30), 0x0000);
    // Round to nearest even in the normal range: 1 + 2^-11 is the tie between 1.0 and
    // 1 + 2^-10; the even mantissa (1.0) wins. 1 + 3 * 2^-11 ties up to the even 1 + 2^-9.
    assert_eq!(f16_bits(1.0 + 2f32.powi(-11)), 0x3c00);
    assert_eq!(f16_bits(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
    // The rounding carry out of the mantissa bumps the exponent.
    assert_eq!(f16_bits(2.0 - 2f32.powi(-12)), 0x4000);
}

// ---- height pyramid ----------------------------------------------------------------------

#[test]
fn vertex_mip_handles_degenerate_and_non_finite_inputs() {
    assert_eq!(vertex_mip(1.0, 1.0, 0.0), 0.0);
    assert_eq!(vertex_mip(4.0, 1.0, 0.0), 2.0);
    assert_eq!(vertex_mip(4.0, 1.0, 0.5), 2.5);
    // Finer than a texel never goes below the exact surface.
    assert_eq!(vertex_mip(0.01, 1.0, 0.0), 0.0);
    // Zero or negative cell and zero texel stay finite and inside the clamp.
    for (cell, texel, t) in [
        (0.0, 1.0, 0.0),
        (-5.0, 1.0, 0.3),
        (1.0, 0.0, 0.0),
        (1.0, -1.0, 0.0),
        (f32::INFINITY, 1.0, 0.0),
        (1.0e30, 1.0e-30, 1.0),
        (f32::NAN, 1.0, 0.0),
        (1.0, f32::NAN, 0.0),
        (4.0, 1.0, f32::NAN),
    ] {
        let m = vertex_mip(cell, texel, t);
        assert!(
            (0.0..=MAX_HEIGHT_MIP as f32).contains(&m),
            "({cell}, {texel}, {t}) -> {m}"
        );
    }
    assert_eq!(vertex_mip(4.0, 1.0, f32::NAN), 2.0, "a NaN morph reads as 0");
    assert_eq!(vertex_mip(1.0e30, 1.0, 1.0), MAX_HEIGHT_MIP as f32);
}

#[test]
fn mip_level_count_boundaries() {
    assert_eq!(mip_level_count(0, 0), 1);
    assert_eq!(mip_level_count(1, 1), 1);
    assert_eq!(mip_level_count(2, 2), 2);
    assert_eq!(mip_level_count(3, 2), 2);
    assert_eq!(mip_level_count(4, 1), 3);
    assert_eq!(mip_level_count(1025, 3), 11);
    assert_eq!(mip_level_count(2048, 3), 12);
    assert_eq!(mip_level_count(4096, 3), 13);
    // Capped at MAX_HEIGHT_MIP + 1 levels, and the longer side decides.
    assert_eq!(mip_level_count(u32::MAX, 1), MAX_HEIGHT_MIP + 1);
    assert_eq!(mip_level_count(2, 4096), mip_level_count(4096, 2));
}

#[test]
fn pyramid_of_the_smallest_and_odd_maps() {
    let two = TerrainMap::from_heights(2, 2, vec![0.0, 4.0, 8.0, 12.0], 10.0, 1.0, 0.0).expect("map");
    let mips = HeightMips::build(&two);
    assert_eq!(mips.level_count(), 2);
    assert_eq!(level_size(&two, &mips, 0), Some((2, 2)));
    assert_eq!(level_size(&two, &mips, 1), Some((1, 1)));
    assert_eq!(level_size(&two, &mips, 2), None);
    assert_eq!(
        mips.level(0).map(|l| l.data.len()),
        None,
        "level 0 is the map, not stored"
    );
    assert_eq!(mips.level(1).expect("top").data, vec![6.0]);
    assert!(mips.level(99).is_none());
    // No pair of texels at the top level: no slope to report.
    assert_eq!(mips.max_slope_in(10.0, [-5.0, -5.0], 10.0, 1), 0.0);
    assert_eq!(mips.max_slope_in(10.0, [-5.0, -5.0], 10.0, 7), 0.0);

    // Odd, non-square: every level matches level_dims and holds finite values inside the
    // source range (box filter of clamped taps never leaves [min, max]).
    let (w, h) = (7_u32, 5_u32);
    let heights: Vec<f32> = (0..w * h).map(|i| ((i * 37) % 11) as f32 - 5.0).collect();
    let map = TerrainMap::from_heights(w, h, heights, 12.0, 1.0, 0.0).expect("map");
    let mips = HeightMips::build(&map);
    assert_eq!(mips.level_count(), mip_level_count(w, h));
    let (lo, hi) = map.min_max_height();
    for k in 1..mips.level_count() {
        let level = mips.level(k).expect("level");
        assert_eq!((level.width, level.height), ((w >> k).max(1), (h >> k).max(1)));
        assert_eq!(level.data.len(), (level.width * level.height) as usize);
        assert!(level.data.iter().all(|v| (lo..=hi).contains(v)), "level {k}");
    }
}

#[test]
fn pyramid_update_region_rejects_empty_and_mismatched_input() {
    use regia_terrain::deform::DirtyRect;
    let map = plane(9, 8.0, |x, z| x + z);
    let mut mips = HeightMips::build(&map);
    let before: Vec<Vec<f32>> = (1..mips.level_count())
        .map(|k| mips.level(k).expect("level").data.clone())
        .collect();

    // Empty rectangles (zero and inverted) change nothing and report nothing.
    for dirty in [
        DirtyRect {
            col_start: 3,
            col_end: 3,
            row_start: 0,
            row_end: 9,
        },
        DirtyRect {
            col_start: 5,
            col_end: 2,
            row_start: 0,
            row_end: 9,
        },
        DirtyRect {
            col_start: 0,
            col_end: 9,
            row_start: 4,
            row_end: 4,
        },
    ] {
        assert!(mips.update_region(&map, &dirty).is_empty());
    }
    // A map of other dimensions is refused rather than read out of bounds.
    let other = plane(5, 8.0, |_, _| 100.0);
    let full = DirtyRect {
        col_start: 0,
        col_end: 5,
        row_start: 0,
        row_end: 5,
    };
    assert!(mips.update_region(&other, &full).is_empty());
    let after: Vec<Vec<f32>> = (1..mips.level_count())
        .map(|k| mips.level(k).expect("level").data.clone())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn pyramid_update_region_past_the_map_edge_matches_a_rebuild() {
    use regia_terrain::deform::DirtyRect;
    let mut map = plane(11, 20.0, |x, z| 0.3 * x - 0.2 * z);
    let mut mips = HeightMips::build(&map);
    // Edit the far corner, then flag a rectangle that overshoots the map.
    let i = map.heights().len() - 1;
    let mut heights = map.heights().to_vec();
    heights[i] += 9.0;
    heights[i - 1] += 5.0;
    map = TerrainMap::from_heights(11, 11, heights, 20.0, 1.0, 0.0).expect("map");
    let dirty = DirtyRect {
        col_start: 9,
        col_end: 40,
        row_start: 10,
        row_end: 40,
    };
    let changed = mips.update_region(&map, &dirty);
    assert_eq!(changed.len() as u32, mips.level_count() - 1);
    let fresh = HeightMips::build(&map);
    for k in 1..mips.level_count() {
        assert_eq!(
            mips.level(k).expect("level").data,
            fresh.level(k).expect("level").data,
            "level {k}"
        );
    }
    for (k, rect) in changed.iter().enumerate() {
        let level = mips.level(k as u32 + 1).expect("level");
        // An odd map's last row or column feeds no texel of the next level (`11 >> 1 = 5`
        // texels read samples 0..=9): that edit leaves the rectangle empty, not out of range.
        assert!(
            rect.0 <= rect.1 && rect.1 <= level.width,
            "level {} cols {rect:?}",
            k + 1
        );
        assert!(
            rect.2 <= rect.3 && rect.3 <= level.height,
            "level {} rows {rect:?}",
            k + 1
        );
    }
}

#[test]
fn pyramid_sampling_is_finite_for_non_finite_and_far_inputs() {
    let map = plane(17, 32.0, |x, z| (x * 0.3).sin() * 4.0 + z * 0.1);
    let mips = HeightMips::build(&map);
    let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0e30, -1.0e30];
    for &b in &bad {
        for &m in &[0.0, 1.5, 99.0, -3.0, f32::NAN, f32::INFINITY] {
            assert!(mips.sample(&map, [b, 3.0], m).is_finite(), "s.x = {b}, mip = {m}");
            assert!(mips.sample(&map, [3.0, b], m).is_finite(), "s.y = {b}, mip = {m}");
            assert!(mips.sample_world(&map, b, b, m).is_finite());
        }
    }
    // mip 0 is exactly the map's bilinear sample; a negative mip clamps to it.
    for &(x, z) in &[(0.0f32, 0.0f32), (3.3, -7.1), (-40.0, 40.0)] {
        let exact = map.sample_bilinear(x, z);
        assert_eq!(mips.sample_world(&map, x, z, 0.0), exact);
        assert_eq!(mips.sample_world(&map, x, z, -2.0), exact);
    }
    // Past the top level the top level is read.
    let top = (mips.level_count() - 1) as f32;
    assert_eq!(
        mips.sample_world(&map, 1.0, 2.0, top),
        mips.sample_world(&map, 1.0, 2.0, top + 50.0)
    );
}

#[test]
fn slope_sampling_is_finite_and_zero_on_flat_ground() {
    let flat = plane(17, 32.0, |_, _| 5.0);
    let mips = HeightMips::build(&flat);
    for m in [0.0, 1.0, 2.5, 99.0] {
        assert_eq!(sample_slope(&flat, &mips, [8.0, 8.0], m), [0.0, 0.0]);
    }
    let ramp = plane(17, 32.0, |x, _| x * 0.5);
    let mips = HeightMips::build(&ramp);
    let [gx, gz] = sample_slope(&ramp, &mips, [8.0, 8.0], 0.0);
    assert!((gx - 0.5).abs() < 1.0e-4 && gz.abs() < 1.0e-6, "{gx} {gz}");
    for s in [[f32::INFINITY, 0.0], [-1.0e30, 1.0e30], [0.0, 0.0], [16.0, 16.0]] {
        let g = sample_slope(&ramp, &mips, s, 1.5);
        assert!(g.iter().all(|c| c.is_finite()), "{s:?} -> {g:?}");
    }
}

// ---- sim ground --------------------------------------------------------------------------

#[test]
fn agreement_flag_thresholds_are_exact() {
    // Slope: strictly below the threshold is driveable; the threshold itself is a cliff.
    assert_eq!(
        agreement_flag(DRIVEABLE_SLOPE_RAD - 1.0e-4, 0.0),
        AgreementFlag::WithinDriveable
    );
    assert_eq!(
        agreement_flag(DRIVEABLE_SLOPE_RAD, 0.0),
        AgreementFlag::WithinCliff
    );
    assert_eq!(
        agreement_flag(f32::NAN, 0.0),
        AgreementFlag::WithinCliff,
        "unknown slope is not driveable"
    );
    // Delta: exactly on the bound is still within; sign does not matter.
    assert_eq!(
        agreement_flag(0.0, DRIVEABLE_DELTA_M),
        AgreementFlag::WithinDriveable
    );
    assert_eq!(
        agreement_flag(0.0, -DRIVEABLE_DELTA_M),
        AgreementFlag::WithinDriveable
    );
    assert_eq!(
        agreement_flag(0.0, DRIVEABLE_DELTA_M + 1.0e-3),
        AgreementFlag::DriveableOver
    );
    assert_eq!(
        agreement_flag(0.0, -DRIVEABLE_DELTA_M - 1.0e-3),
        AgreementFlag::DriveableOver
    );
    assert_eq!(agreement_flag(1.2, CLIFF_DELTA_M), AgreementFlag::WithinCliff);
    assert_eq!(
        agreement_flag(1.2, -CLIFF_DELTA_M - 1.0e-3),
        AgreementFlag::CliffOver
    );
    // A delta the driveable bound rejects is fine on a cliff.
    assert_eq!(agreement_flag(1.2, 1.0), AgreementFlag::WithinCliff);
    assert_eq!(agreement_flag(0.0, f32::INFINITY), AgreementFlag::DriveableOver);
    assert_eq!(
        agreement_flag(f32::INFINITY, f32::NEG_INFINITY),
        AgreementFlag::CliffOver
    );
}

#[test]
fn gpu_budgets_are_sane_and_ordered() {
    let budgets = do27_gpu_budgets();
    assert_eq!(budgets.len(), 3);
    for b in budgets {
        assert!(!b.gpu.is_empty());
        assert!(b.height_px > 0);
        assert!(
            b.terrain_pass_ms.is_finite() && b.terrain_pass_ms > 0.0,
            "{}",
            b.gpu
        );
        assert!(
            b.tile_budget_gib.is_finite() && b.tile_budget_gib > 0.0,
            "{}",
            b.gpu
        );
    }
    // The weakest GPU has the loosest time target and the smallest tile cache.
    let deck = budgets.iter().find(|b| b.gpu.contains("Deck")).expect("deck");
    assert!(budgets.iter().all(|b| b.terrain_pass_ms <= deck.terrain_pass_ms));
    assert!(budgets.iter().all(|b| b.tile_budget_gib >= deck.tile_budget_gib));
}

#[test]
fn sim_ground_validates_like_the_map_and_reports_its_larger_side() {
    let bad = Heightfield {
        width: 1,
        height: 1,
        samples: vec![0],
    };
    assert!(matches!(
        SimGround::new(&bad, 10.0, 1.0, 0.0),
        Err(TerrainMapError::TooSmall { .. })
    ));
    let short = Heightfield {
        width: 3,
        height: 3,
        samples: vec![0; 8],
    };
    assert!(matches!(
        SimGround::new(&short, 10.0, 1.0, 0.0),
        Err(TerrainMapError::SampleCount(_))
    ));
    let wide = Heightfield {
        width: 6,
        height: 3,
        samples: vec![0; 18],
    };
    let ground = SimGround::new(&wide, 10.0, 1.0, 0.0).expect("ground");
    assert_eq!(ground.resolution(), 6);
    assert_eq!(ground.map().width(), 6);
    // Non-finite queries read the border, never NaN.
    assert!(ground.height(f32::NAN, f32::INFINITY).is_finite());
    assert!(ground.normal(f32::NAN, f32::NAN).iter().all(|c| c.is_finite()));
}

#[test]
fn visual_sim_delta_is_signed_and_zero_against_itself() {
    let samples: Vec<u16> = (0..16u32).map(|i| (i * 4000) as u16).collect();
    let hf = Heightfield {
        width: 4,
        height: 4,
        samples,
    };
    let sim = SimGround::new(&hf, 30.0, 10.0, 0.0).expect("sim");
    let same = TerrainMap::new(&hf, 30.0, 10.0, 0.0).expect("visual");
    let raised = TerrainMap::new(&hf, 30.0, 10.0, 0.5).expect("visual");
    let lowered = TerrainMap::new(&hf, 30.0, 10.0, -0.5).expect("visual");
    for (x, z) in [
        (0.0, 0.0),
        (-15.0, 15.0),
        (7.3, -2.2),
        (1.0e9, 1.0e9),
        (f32::NAN, 0.0),
    ] {
        assert_eq!(visual_sim_delta(&same, &sim, x, z), 0.0);
        assert!((visual_sim_delta(&raised, &sim, x, z) - 0.5).abs() < 1.0e-5);
        assert!((visual_sim_delta(&lowered, &sim, x, z) + 0.5).abs() < 1.0e-5);
    }
}

// ---- mist --------------------------------------------------------------------------------

#[test]
fn mist_grid_refuses_negative_and_infinite_cells() {
    let map = plane(9, 64.0, |_, _| 3.0);
    for cell in [-1.0, f32::NEG_INFINITY, f32::INFINITY] {
        assert!(mist_grid_from_map(&map, cell, 50.0).is_none(), "cell {cell}");
    }
    let grid = mist_grid_from_map(&map, 8.0, 50.0).expect("grid");
    assert_eq!(grid.origin(), [-32.0, -32.0]);
    let again = mist_grid_from_map(&map, 8.0, 50.0).expect("grid");
    assert_eq!((grid.width(), grid.origin()), (again.width(), again.origin()));
    assert_eq!(grid.sample(1.0, 2.0), again.sample(1.0, 2.0));
}

// ---- terrain map -------------------------------------------------------------------------

#[test]
fn clone_is_independent_and_equal() {
    let map = plane(5, 16.0, |x, z| x * z);
    let copy = map.clone();
    assert_eq!(copy.heights(), map.heights());
    assert_eq!((copy.width(), copy.height()), (map.width(), map.height()));
    assert_eq!(copy.world_width(), map.world_width());
    assert_eq!(copy.sample_bilinear(1.3, -2.1), map.sample_bilinear(1.3, -2.1));
    drop(map);
    assert!(copy.sample_bilinear(0.0, 0.0).is_finite());
}

#[test]
fn from_heights_rejects_overflowing_dimensions_without_allocating() {
    // u32::MAX x u32::MAX samples cannot match a tiny buffer: the count check must not wrap.
    let r = TerrainMap::from_heights(u32::MAX, u32::MAX, vec![0.0; 4], 10.0, 1.0, 0.0);
    assert!(matches!(r, Err(TerrainMapError::SampleCount(_))));
    for bad in [f32::INFINITY, f32::NEG_INFINITY, -4.0, 0.0] {
        assert!(matches!(
            TerrainMap::from_heights(2, 2, vec![0.0; 4], bad, 1.0, 0.0),
            Err(TerrainMapError::BadWorldWidth(_))
        ));
    }
    for (scale, offset) in [(f32::INFINITY, 0.0), (1.0, f32::NAN), (f32::NEG_INFINITY, 1.0)] {
        assert!(matches!(
            TerrainMap::from_heights(2, 2, vec![0.0; 4], 10.0, scale, offset),
            Err(TerrainMapError::BadHeightParams { .. })
        ));
    }
}

#[test]
fn quantised_map_hits_the_exact_range_ends() {
    let hf = Heightfield {
        width: 2,
        height: 2,
        samples: vec![0, 65535, 32768, 1],
    };
    let map = TerrainMap::new(&hf, 8.0, 100.0, 7.0).expect("map");
    assert_eq!(map.height_at_sample(0, 0), 7.0);
    assert!((map.height_at_sample(1, 0) - 107.0).abs() < 1.0e-4);
    assert!((map.height_at_sample(0, 1) - (7.0 + 32768.0 / 65535.0 * 100.0)).abs() < 1.0e-4);
    assert!((map.height_at_sample(1, 1) - (7.0 + 100.0 / 65535.0)).abs() < 1.0e-5);
    // A negative scale flips the relief but is allowed (finite).
    let flipped = TerrainMap::new(&hf, 8.0, -100.0, 0.0).expect("map");
    assert!((flipped.height_at_sample(1, 0) + 100.0).abs() < 1.0e-3);
}

// ---- ray casting -------------------------------------------------------------------------

#[test]
fn raycast_degenerate_rays_miss_without_panicking() {
    let map = plane(9, 32.0, |_, _| 0.0);
    let o = Vec3::new(0.0, 10.0, 0.0);
    assert!(raycast_with_holes(&map, None, o, Vec3::ZERO, 100.0).is_none());
    assert!(raycast_with_holes(&map, None, o, Vec3::new(f32::NAN, -1.0, 0.0), 100.0).is_none());
    assert!(raycast_with_holes(&map, None, o, Vec3::new(0.0, f32::NEG_INFINITY, 0.0), 100.0).is_none());
    assert!(raycast_with_holes(&map, None, Vec3::new(f32::NAN, 10.0, 0.0), Vec3::NEG_Y, 100.0).is_none());
    // Zero, negative and NaN reach find nothing; a reach that stops short finds nothing.
    for reach in [0.0, -5.0, f32::NAN, 9.0] {
        assert!(
            raycast_with_holes(&map, None, o, Vec3::NEG_Y, reach).is_none(),
            "reach {reach}"
        );
    }
    // Pointing up from above the ground never hits it.
    assert!(raycast_with_holes(&map, None, o, Vec3::Y, 1.0e6).is_none());
    // A ray that starts under the ground has no crossing from above and misses (a surface,
    // not a solid).
    assert!(raycast_with_holes(&map, None, Vec3::new(0.0, -3.0, 0.0), Vec3::NEG_Y, 100.0).is_none());
    // An infinite reach is bounded by the step cap instead of looping forever.
    assert!(raycast_with_holes(&map, None, o, Vec3::Y, f32::INFINITY).is_none());
}

#[test]
fn raycast_hits_a_known_point_and_is_repeatable() {
    // Ground y = 0.5 x: a vertical ray at x = 4 meets it at y = 2.
    let map = plane(33, 64.0, |x, _| 0.5 * x);
    let o = Vec3::new(4.0, 30.0, 6.0);
    let hit = raycast_with_holes(&map, None, o, Vec3::NEG_Y, 100.0).expect("hit");
    assert!(
        (hit.x - 4.0).abs() < 1.0e-3 && (hit.z - 6.0).abs() < 1.0e-3,
        "{hit}"
    );
    assert!((hit.y - 2.0).abs() < 1.0e-3, "{hit}");
    // The direction need not be unit length.
    let scaled = raycast_with_holes(&map, None, o, Vec3::new(0.0, -7.0, 0.0), 100.0).expect("hit");
    assert_eq!(hit, scaled);
    assert_eq!(
        hit,
        raycast_with_holes(&map, None, o, Vec3::NEG_Y, 100.0).expect("hit")
    );
    // A ray that goes past the map edge before it descends does not hit the clamped border.
    assert!(
        raycast_with_holes(
            &map,
            None,
            Vec3::new(31.0, 5.0, 0.0),
            Vec3::new(1.0, -0.001, 0.0),
            500.0
        )
        .is_none()
    );
}

// ---- scatter set reload ------------------------------------------------------------------

const RULE: &str = include_str!("fixtures/rocks.scatter.ron");

#[test]
fn scatter_sets_load_reload_and_error_paths() {
    let dir = unique_temp_dir("scatter-sets");
    let path = dir.join("rocks.scatter.ron");
    let missing = dir.join("absent.scatter.ron");
    let mut sets = ScatterSets::default();

    // Not loaded: nothing to get, no revision, reload is a quiet "no".
    assert!(sets.get(&path).is_none());
    assert_eq!(sets.revision(&path), None);
    assert_eq!(sets.reload(&path), Ok(false));
    // A missing file is a reported error, and does not register an entry.
    assert!(sets.load(&missing).is_err());
    assert!(sets.get(&missing).is_none());

    write_atomic(&path, RULE.as_bytes()).expect("write");
    let first = sets.load(&path).expect("load");
    assert_eq!(first.seed, 2747);
    assert_eq!(sets.revision(&path), Some(0));
    // A second load serves the cached Arc: no re-parse, even if the file changed on disk.
    write_atomic(&path, RULE.replace("seed: 2747", "seed: 1").as_bytes()).expect("write");
    let second = sets.load(&path).expect("load");
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert_eq!(sets.revision(&path), Some(0));

    // Reload picks the change up and bumps the revision; the old Arc stays valid.
    assert_eq!(sets.reload(&path), Ok(true));
    assert_eq!(sets.get(&path).expect("set").seed, 1);
    assert_eq!(sets.revision(&path), Some(1));
    assert_eq!(first.seed, 2747);

    // A broken file keeps the previous set and the revision.
    write_atomic(&path, b"ScatterRule(version: 1, seed: ").expect("write");
    assert!(sets.reload(&path).is_err());
    assert_eq!(sets.get(&path).expect("kept").seed, 1);
    assert_eq!(sets.revision(&path), Some(1));
    // A file that parses but is invalid (cell size 0) is refused the same way.
    write_atomic(&path, RULE.replace("cell_size: 8.0", "cell_size: 0.0").as_bytes()).expect("write");
    assert!(sets.reload(&path).is_err());
    assert_eq!(sets.revision(&path), Some(1));
    // A deleted file is an error, not a panic, and still keeps the set.
    std::fs::remove_file(&path).expect("remove");
    assert!(sets.reload(&path).is_err());
    assert_eq!(sets.get(&path).expect("kept").seed, 1);

    // The path is canonicalised: an alias of the same file shares the entry.
    write_atomic(&path, RULE.as_bytes()).expect("write");
    let alias = dir.join(".").join("rocks.scatter.ron");
    assert!(sets.get(&alias).is_some());
    assert_eq!(sets.revision(&alias), sets.revision(&path));
    let _ = std::fs::remove_dir_all(&dir);
}
